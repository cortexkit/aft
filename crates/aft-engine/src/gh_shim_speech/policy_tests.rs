use super::*;
use sha2::Digest;

#[test]
fn invalid_signed_speech_manifest_warns_without_envelope_contents() {
    let paths = StatePaths::from_process();
    crate::gh_shim_speech::tests::assert_snapshot_warning(
        || verified_speech_manifest_at(b"secret-invalid-envelope", &paths).unwrap_err(),
        "gh_shim::verified_speech_manifest",
        "envelope_json",
        json!({"envelope_bytes": 23}),
    );
    let mut envelope = super::tests::signed(&super::tests::fixture_manifest(), unix_seconds());
    envelope.envelope_version += 1;
    let bytes = serde_json::to_vec(&envelope).unwrap();
    crate::gh_shim_speech::tests::assert_snapshot_warning(
        || verified_speech_manifest_at(&bytes, &paths).unwrap_err(),
        "gh_shim::verified_speech_manifest",
        "envelope_version",
        json!({"envelope_bytes": bytes.len()}),
    );
}

fn snapshot(manifest: &Manifest) -> crate::gh_shim_speech::model::PolicySnapshot {
    let bytes = serde_json::to_vec(&super::tests::signed(manifest, unix_seconds())).unwrap();
    crate::gh_shim_speech::model::PolicySnapshot {
        artifact_id: manifest.artifact_id.clone(),
        version: manifest.manifest_version,
        digest: sha2::Sha256::digest(&bytes).to_vec(),
        signed_envelope: bytes,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn comment_input(manifest: &Manifest, body: &Path) -> (Vec<OsString>, AgentBinding) {
    let (repo, agent) = manifest.bindings.iter().next().unwrap();
    (
        vec![
            "issue".into(),
            "comment".into(),
            "7".into(),
            "--repo".into(),
            repo.into(),
            "--body-file".into(),
            body.as_os_str().to_owned(),
        ],
        AgentBinding {
            repo: repo.clone(),
            agent_id: agent.clone(),
        },
    )
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn frozen_body_is_replayed_under_both_signed_envelopes_without_reopening_the_file() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = super::tests::fixture_manifest();
    let dir = tempfile::tempdir().unwrap();
    super::tests::write_fixture_pin(&StatePaths::from_root(dir.path().to_path_buf()));
    let _state = Env::set(GH_SHIM_STATE_DIR_ENV, Some(dir.path().as_os_str()));
    let file = dir.path().join("body");
    std::fs::write(&file, "original frozen body").unwrap();
    let (args, binding) = comment_input(&manifest, &file);
    let Classification::Governed { tuple, canonical } =
        classify(&args, &manifest, current_platform())
    else {
        panic!("fixture must govern comments")
    };
    let request =
        canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version).unwrap();
    let invocation = request.invocation.clone().unwrap();
    let rung = RungDetermination::r3(
        0,
        manifest.manifest_version,
        &RungRecordProvenance {
            image_path: "/aft".into(),
            version: "test".into(),
            repo_key: binding.repo.clone(),
        },
    );
    let wire = governed_wire_request(&rung.record, &binding.agent_id, request);
    std::fs::write(&file, "changed file must never be read by the daemon").unwrap();
    let start = snapshot(&manifest);
    let mut newer = manifest.clone();
    newer.manifest_version += 1;
    validate_speech_policy(&start, &snapshot(&newer), &wire, &invocation).unwrap();
    let mut missing = invocation.clone();
    missing.body_inputs.clear();
    assert_eq!(
        validate_speech_policy(&start, &snapshot(&newer), &wire, &missing)
            .unwrap_err()
            .reason,
        "canonicalization_changed"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn a_current_only_classification_cannot_expand_the_start_envelope() {
    let _lock = crate::test_env::process_env_lock();
    let mut old = super::tests::fixture_manifest();
    let dir = tempfile::tempdir().unwrap();
    super::tests::write_fixture_pin(&StatePaths::from_root(dir.path().to_path_buf()));
    let _state = Env::set(GH_SHIM_STATE_DIR_ENV, Some(dir.path().as_os_str()));
    let file = dir.path().join("body");
    std::fs::write(&file, "body").unwrap();
    let (args, binding) = comment_input(&old, &file);
    let Classification::Governed { tuple, canonical } = classify(&args, &old, current_platform())
    else {
        panic!("governed fixture")
    };
    let request = canonicalize_governed(&args, &tuple, &canonical, old.manifest_version).unwrap();
    let invocation = request.invocation.clone().unwrap();
    let rung = RungDetermination::r3(
        0,
        old.manifest_version,
        &RungRecordProvenance {
            image_path: "/aft".into(),
            version: "test".into(),
            repo_key: binding.repo.clone(),
        },
    );
    let wire = governed_wire_request(&rung.record, &binding.agent_id, request);
    let current = snapshot(&old);
    for rows in old.tiers.values_mut() {
        rows.retain(|row| row.tuple() != "issue comment");
    }
    assert!(validate_speech_policy(&snapshot(&old), &current, &wire, &invocation).is_err());
}

#[test]
fn protected_write_detection_does_not_turn_mechanical_commands_into_writes() {
    for args in [
        vec!["--version"],
        vec!["--help"],
        vec!["issue", "list"],
        vec!["api", "-X", "GET", "/repos/owner/repo/issues"],
    ] {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        assert!(!speech_protected_without_view(&args), "{args:?}");
    }
    for args in [
        vec!["issue", "comment", "7", "--body", "hello"],
        vec!["auth", "token"],
        vec!["api", "-X", "POST", "/repos/owner/repo/issues"],
    ] {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        assert!(speech_protected_without_view(&args), "{args:?}");
    }
}

struct Env(&'static str, Option<OsString>);
impl Env {
    fn set(key: &'static str, value: Option<&OsStr>) -> Self {
        let old = std::env::var_os(key);
        match value {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
        Self(key, old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => unsafe { std::env::set_var(self.0, value) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

#[test]
fn protected_manifest_and_operator_arms_delegate_only_on_operator_disable() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = super::tests::fixture_manifest();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let xdg = dir.path().join("config");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _home = Env::set("HOME", Some(home.as_os_str()));
    let _profile = Env::set("USERPROFILE", Some(home.as_os_str()));
    let _xdg = Env::set("XDG_CONFIG_HOME", Some(xdg.as_os_str()));
    let _state = Env::set("AFT_GH_SHIM_STATE_DIR", Some(state.as_os_str()));
    let _provisioned = Env::set(
        crate::gh_shim_speech::start_view::PROVISIONED_ENV,
        Some(OsStr::new("1")),
    );
    let token = "11111111111111111111111111111111";
    let _ticket = Env::set(
        crate::gh_shim_paths::GH_SHIM_TICKET_ENV,
        Some(OsStr::new(token)),
    );
    let view = dir.path().join("start.json");
    std::fs::write(&view,serde_json::to_vec(&json!({"ticket_digest":crate::gh_shim_ticket::speech_digest(token).unwrap().to_vec(),"policy":snapshot(&manifest)})).unwrap()).unwrap();
    let _view = Env::set(
        crate::gh_shim_speech::start_view::START_VIEW_ENV,
        Some(view.as_os_str()),
    );
    let paths = StatePaths::from_root(state.clone());
    super::tests::write_fixture_pin(&paths);
    write_version_high_water(&paths, manifest.manifest_version);
    let repo = manifest.bindings.keys().next().unwrap();
    let args: Vec<OsString> = ["issue", "comment", "7", "--repo", repo, "--body", "body"]
        .into_iter()
        .map(Into::into)
        .collect();
    for mode in [
        "dormant",
        "invalid",
        "regressed",
        "operator-disable",
        "operator-credential",
        "unbound-write",
        "operator-bypass",
        "non-r3",
    ] {
        let _ = std::fs::remove_file(&paths.manifest);
        let _ = std::fs::remove_file(&paths.last_valid_manifest);
        let config = crate::subc_config::user_config_path_from(
            Some(xdg.as_os_str()),
            Some(home.as_os_str()),
        )
        .unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            if mode == "operator-disable" {
                "{\"github\":{\"shim\":false}}"
            } else {
                "{\"github\":{\"shim\":true}}"
            },
        )
        .unwrap();
        if matches!(mode, "invalid" | "regressed") {
            std::fs::write(&paths.manifest, "invalid-json").unwrap();
        }
        if mode == "regressed" {
            write_last_valid_manifest(&paths, &manifest, &[]);
        }
        if matches!(
            mode,
            "operator-disable"
                | "operator-credential"
                | "unbound-write"
                | "operator-bypass"
                | "non-r3"
        ) {
            super::tests::write_signed_manifest(&paths, manifest.clone(), unix_seconds());
        }
        let _bypass = Env::set(
            "GH_SHIM_BYPASS",
            (mode == "operator-bypass").then(|| OsStr::new("operator")),
        );
        let invocation: Vec<OsString> = if mode == "operator-credential" {
            vec!["auth".into(), "token".into()]
        } else if mode == "unbound-write" {
            vec![
                "issue".into(),
                "comment".into(),
                "7".into(),
                "--repo".into(),
                "unbound/repo".into(),
                "--body".into(),
                "body".into(),
            ]
        } else {
            args.clone()
        };
        let plan = captured_plan(
            &invocation,
            &paths,
            &TargetRepository::from_invocation(&invocation),
            dir.path(),
            unix_seconds(),
        );
        let expected = match mode {
            "unbound-write" | "non-r3" if current_platform() == "unsupported" => "not_governed",
            "operator-disable" | "non-r3" => "not_r3",
            "operator-credential" | "operator-bypass" => "not_governed",
            "unbound-write" => "repository_denied",
            _ => "current_manifest_unverifiable",
        };
        assert_eq!(
            plan.err()
                .expect("protected preflight must refuse")
                .refusal()
                .reason,
            expected,
            "{mode}"
        );
        let delegated = std::cell::Cell::new(false);
        let exit = run_with(
            &invocation,
            |_| {
                delegated.set(true);
                0
            },
            |_, _, _, _| Ok(None),
        );
        if mode == "operator-disable" {
            assert_eq!(exit, 0, "explicit operator disable must pass through");
            assert!(delegated.get());
        } else {
            assert_ne!(exit, 0, "{mode}");
            assert!(
                !delegated.get(),
                "protected {mode} must not reach native gh"
            );
        }
    }
    for invalid_view in ["missing", "corrupt", "wrong-ticket"] {
        match invalid_view {
            "missing" => std::fs::remove_file(&view).unwrap(),
            "corrupt" => std::fs::write(&view, b"not JSON").unwrap(),
            "wrong-ticket" => std::fs::write(&view, serde_json::to_vec(&json!({
                "ticket_digest": crate::gh_shim_ticket::speech_digest("22222222222222222222222222222222").unwrap().to_vec(),
                "policy": snapshot(&manifest),
            })).unwrap()).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            crate::gh_shim_speech::start_view::protected(&args),
            "{invalid_view}"
        );
        let delegated = std::cell::Cell::new(false);
        let exit = run_with(
            &args,
            |_| {
                delegated.set(true);
                0
            },
            |_, _, _, _| Ok(None),
        );
        assert_ne!(exit, 0, "{invalid_view}");
        assert!(
            !delegated.get(),
            "{invalid_view} start view must not delegate to ambient gh"
        );
    }
    assert!(!crate::gh_shim_speech::start_view::protected(&[
        "--version".into()
    ]));
}

#[cfg(debug_assertions)]
fn with_acceptance_store(test: impl FnOnce(&StatePaths, &crate::gh_shim_speech::service::Service)) {
    let _lock = crate::test_env::process_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("shim-state");
    fs::create_dir_all(&state).unwrap();
    let _state = Env::set("AFT_GH_SHIM_STATE_DIR", Some(state.as_os_str()));
    let root = dir.path().join("daemon-store");
    fs::create_dir_all(&root).unwrap();
    crate::db::open(&root.join("aft.db")).unwrap();
    let service = crate::gh_shim_speech::service::record_service(&root);
    test(&StatePaths::from_root(state), &service);
}

#[cfg(debug_assertions)]
fn accept(paths: &StatePaths, manifest: &Manifest) {
    super::tests::write_signed_manifest(paths, manifest.clone(), unix_seconds());
    load_manifest(paths, unix_seconds()).unwrap();
}

#[test]
#[cfg(debug_assertions)]
fn shim_acceptance_registers_verified_policy_and_monotonic_upgrade() {
    with_acceptance_store(|paths, service| {
        let mut manifest = super::tests::fixture_manifest();
        accept(paths, &manifest);
        let (first, _) = service.register_accepted_policy().unwrap().unwrap();
        assert_eq!(first.version, manifest.manifest_version);
        assert_eq!(first.signed_envelope, fs::read(&paths.manifest).unwrap());
        assert_eq!(service.store.latest_policy().unwrap(), Some(first.clone()));
        manifest.manifest_version += 1;
        accept(paths, &manifest);
        let (next, _) = service.register_accepted_policy().unwrap().unwrap();
        assert_eq!(next.version, first.version + 1);
        assert_ne!(next.digest, first.digest);
        assert_eq!(service.store.latest_policy().unwrap(), Some(next));
    });
}

#[test]
#[cfg(debug_assertions)]
fn missing_shim_acceptance_cannot_initialize_speech_history() {
    with_acceptance_store(|paths, service| {
        let manifest = super::tests::fixture_manifest();
        super::tests::write_signed_manifest(paths, manifest.clone(), unix_seconds());
        write_version_high_water(paths, manifest.manifest_version);
        assert!(service.register_accepted_policy().is_err());
        assert_eq!(service.store.latest_policy().unwrap(), None);
        accept(paths, &manifest);
        service.register_accepted_policy().unwrap();
        fs::remove_file(&paths.last_valid_manifest).unwrap();
        assert!(service.register_accepted_policy().is_err());
        assert_eq!(
            service.store.latest_policy().unwrap().unwrap().version,
            manifest.manifest_version
        );
    });
}

#[test]
#[cfg(debug_assertions)]
fn rolled_back_shim_acceptance_cannot_lower_daemon_watermark() {
    with_acceptance_store(|paths, service| {
        let mut manifest = super::tests::fixture_manifest();
        accept(paths, &manifest);
        let old_record = fs::read(&paths.last_valid_manifest).unwrap();
        let old_envelope = fs::read(&paths.manifest).unwrap();
        manifest.manifest_version += 1;
        accept(paths, &manifest);
        service.register_accepted_policy().unwrap();
        fs::write(&paths.last_valid_manifest, old_record).unwrap();
        fs::write(&paths.manifest, old_envelope).unwrap();
        write_version_high_water(paths, manifest.manifest_version - 1);
        assert_eq!(
            service.register_accepted_policy().unwrap_err().reason,
            "manifest_rollback"
        );
        assert_eq!(
            service.store.latest_policy().unwrap().unwrap().version,
            manifest.manifest_version
        );
    });
}

#[test]
#[cfg(debug_assertions)]
fn same_version_accepted_digest_conflict_does_not_replace_speech_history() {
    with_acceptance_store(|paths, service| {
        let mut manifest = super::tests::fixture_manifest();
        accept(paths, &manifest);
        let (first, _) = service.register_accepted_policy().unwrap().unwrap();
        manifest.issued_at_unix_secs -= 1;
        accept(paths, &manifest);
        assert_eq!(
            service.register_accepted_policy().unwrap_err().reason,
            "manifest_rollback"
        );
        assert_eq!(service.store.latest_policy().unwrap(), Some(first));
    });
}

#[test]
#[cfg(debug_assertions)]
fn tampered_accepted_envelope_cannot_provision_speech_history() {
    with_acceptance_store(|paths, service| {
        let manifest = super::tests::fixture_manifest();
        accept(paths, &manifest);
        let mut accepted: LastValidManifest =
            serde_json::from_slice(&fs::read(&paths.last_valid_manifest).unwrap()).unwrap();
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&accepted.signed_envelope).unwrap();
        envelope["signature"] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode([0_u8; 64]));
        accepted.signed_envelope = serde_json::to_vec(&envelope).unwrap();
        fs::write(&paths.manifest, &accepted.signed_envelope).unwrap();
        fs::write(
            &paths.last_valid_manifest,
            serde_json::to_vec(&accepted).unwrap(),
        )
        .unwrap();
        assert!(service.register_accepted_policy().is_err());
        assert_eq!(service.store.latest_policy().unwrap(), None);
    });
}

struct RunWithFixture {
    _env: Vec<Env>,
    _dir: tempfile::TempDir,
    paths: StatePaths,
    view: PathBuf,
}

impl RunWithFixture {
    fn new(manifest: &Manifest) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let xdg = dir.path().join("config");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let paths = StatePaths::from_root(state.clone());
        let view = dir.path().join("start.json");
        let token = "11111111111111111111111111111111";
        let env = vec![
            Env::set("HOME", Some(home.as_os_str())),
            Env::set("USERPROFILE", Some(home.as_os_str())),
            Env::set("XDG_CONFIG_HOME", Some(xdg.as_os_str())),
            Env::set("AFT_GH_SHIM_STATE_DIR", Some(state.as_os_str())),
            Env::set("GH_SHIM_BYPASS", None),
            Env::set(crate::gh_shim_speech::start_view::PROVISIONED_ENV, None),
            Env::set(crate::gh_shim_speech::start_view::WORKER_POLICY_ENV, None),
            Env::set(
                crate::gh_shim_speech::start_view::START_VIEW_ENV,
                Some(view.as_os_str()),
            ),
            Env::set(
                crate::gh_shim_paths::GH_SHIM_TICKET_ENV,
                Some(OsStr::new(token)),
            ),
        ];
        let config = crate::subc_config::user_config_path_from(
            Some(xdg.as_os_str()),
            Some(home.as_os_str()),
        )
        .unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, r#"{"github":{"shim":true}}"#).unwrap();
        super::tests::write_signed_manifest(&paths, manifest.clone(), unix_seconds());
        load_manifest(&paths, unix_seconds()).unwrap();
        std::fs::write(
            &view,
            serde_json::to_vec(&json!({
                "ticket_digest": crate::gh_shim_ticket::speech_digest(token).unwrap().to_vec(),
                "policy": snapshot(manifest),
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            _env: env,
            _dir: dir,
            paths,
            view,
        }
    }
}

fn exact_policy_refusal(reason: &str) -> String {
    let mut text = format!("gh-shim: gh_shim_seam_refusal: task_gh_policy_denied: this GitHub write is not allowed by the routing policy the command started under, or by the current one ({reason}); use a permitted operation, or ask the operator to update the policy and start the command again.");
    if reason == "not_governed" {
        text.push_str(" This GitHub command is not a bot operation. If it should run as the operator, run it again with GH_SHIM_BYPASS=operator; the shim records that use.");
    }
    text
}

#[test]
fn captured_rung_refusals_distinguish_discovery_failures_from_policy() {
    use crate::gh_shim_speech::Code;
    let r1_cases = [
        (R1Reason::DisabledByConfig, false),
        (R1Reason::AbsentOrUnparseable, false),
        (R1Reason::Unreachable, true),
        (R1Reason::DiscoveryBudgetExhausted, true),
    ];
    let r2_cases = [
        (R2Reason::ManifestUnavailable, false),
        (R2Reason::AgentBindingUnavailable, false),
        (R2Reason::AgentCredentialsPresent, false),
        (R2Reason::DaemonUnreachable, true),
        (R2Reason::CatalogGhRouteAbsent, true),
        (R2Reason::GhRouteHolderUnbound, true),
    ];
    assert_eq!(r1_cases.len(), R1Reason::ALL.len());
    assert_eq!(r2_cases.len(), R2Reason::ALL.len());
    let provenance = RungRecordProvenance {
        image_path: "aft".to_string(),
        version: "test".to_string(),
        repo_key: "cortexkit/aft".to_string(),
    };
    let cases = r1_cases
        .into_iter()
        .map(|(reason, temporary)| (RungDetermination::r1(1, reason), temporary))
        .chain(r2_cases.into_iter().map(|(reason, temporary)| {
            (
                RungDetermination::r2(1, reason, Some(1), &provenance),
                temporary,
            )
        }));
    for (determination, temporary) in cases {
        let refusal = captured_rung_refusal(&determination.record);
        let expected = if temporary {
            (Code::AuthorityUnavailable, "relay_unavailable")
        } else {
            (Code::PolicyDenied, "not_r3")
        };
        assert_eq!(
            (refusal.code, refusal.reason.as_str()),
            expected,
            "{:?}",
            determination.record.inputs
        );
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn protected_writes_refuse_during_restart_but_operator_disable_passes_through() {
    let _lock = crate::test_env::process_env_lock();
    for disabled in [false, true] {
        let manifest = super::tests::fixture_manifest();
        let fixture = RunWithFixture::new(&manifest);
        let _provisioned = Env::set(
            crate::gh_shim_speech::start_view::PROVISIONED_ENV,
            Some(OsStr::new("1")),
        );
        let repo = manifest.bindings.keys().next().unwrap();
        let args: Vec<OsString> = ["issue", "comment", "7", "--repo", repo, "--body", "body"]
            .into_iter()
            .map(Into::into)
            .collect();
        let target = TargetRepository::from_invocation(&args);
        let cwd = std::env::current_dir().unwrap();
        let now = unix_seconds();
        let connection_file = fixture._dir.path().join("connection.json");
        std::fs::write(&connection_file, "present").unwrap();
        let config = crate::subc_config::user_config_path_from(
            Some(fixture._dir.path().join("config").as_os_str()),
            Some(fixture._dir.path().join("home").as_os_str()),
        )
        .unwrap();
        std::fs::write(
            config,
            serde_json::to_vec(&json!({
                "github": { "shim": !disabled },
                "subc": { "connection_file": connection_file },
            }))
            .unwrap(),
        )
        .unwrap();
        // Discovery caches the missing relay while AFT restarts. Reusing that
        // record must refuse with retry advice, not claim the policy changed.
        let cached = RungDetermination::r2(
            now,
            R2Reason::CatalogGhRouteAbsent,
            None,
            &RungRecordProvenance::for_target(&target, &cwd),
        );
        write_rung_record_silently(&fixture.paths, &cached.record);
        let determination = determine_rung(&fixture.paths, &target, &cwd, now);
        if disabled {
            assert_eq!(determination.record.rung, Rung::R1);
            assert!(determination.operator_disabled);
        } else {
            assert_eq!(determination.record.rung, Rung::R2);
            assert_eq!(determination.record.inputs, cached.record.inputs);
        }
        let expected = if disabled {
            exact_policy_refusal("not_r3")
        } else {
            "gh-shim: gh_shim_seam_refusal: task_gh_authority_unavailable: bot writes from background commands are briefly unavailable (relay_unavailable); retry in a few seconds.".to_string()
        };
        let plan = captured_plan(&args, &fixture.paths, &target, &cwd, now);
        let refusal = plan
            .err()
            .expect("non-R3 write must not reach dispatch")
            .refusal();
        assert_eq!(
            format!("gh-shim: gh_shim_seam_refusal: {}", refusal.agent_text()),
            expected
        );
        let (_capture, lines) = capture_refusals_for_test();
        let delegated = std::cell::Cell::new(false);
        let exit = run_with(
            &args,
            |_| {
                delegated.set(true);
                0
            },
            |_, _, _, _| panic!("protected refusal must not reach the native probe"),
        );
        if disabled {
            assert_eq!(exit, 0);
            assert!(delegated.get());
            assert!(lines.lock().unwrap().is_empty());
        } else {
            assert_eq!(exit, REFUSAL_EXIT_STATUS);
            assert!(!delegated.get());
            assert_eq!(*lines.lock().unwrap(), vec![expected]);
        }
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn provisioned_head_run_rerun_names_the_audited_operator_bypass() {
    let _lock = crate::test_env::process_env_lock();
    let mut manifest: Manifest = serde_json::from_str(include_str!(
        "../../../aft/tests/fixtures/gh_shim/v10-manifest.json"
    ))
    .unwrap();
    manifest.tiers.get_mut(&Tier::Admin).unwrap().push(
        serde_json::from_value(json!({ "tuple": "run rerun", "platform": ["macos", "linux"] }))
            .unwrap(),
    );
    let _fixture = RunWithFixture::new(&manifest);
    let _provisioned = Env::set(
        crate::gh_shim_speech::start_view::PROVISIONED_ENV,
        Some(OsStr::new("1")),
    );
    let args: Vec<OsString> = ["run", "rerun", "123", "--failed", "--repo", "cortexkit/aft"]
        .into_iter()
        .map(Into::into)
        .collect();
    assert!(matches!(
        classify(&args, &manifest, current_platform()),
        Classification::Admin { .. }
    ));
    let (_capture, lines) = capture_refusals_for_test();
    let exit = run_with(
        &args,
        |_| panic!("head write delegated without bypass"),
        |_, _, _, _| panic!("head refusal must not need a probe"),
    );
    assert_eq!(exit, REFUSAL_EXIT_STATUS);
    assert_eq!(*lines.lock().unwrap(), vec!["gh-shim: gh_shim_seam_refusal: task_gh_policy_denied: this GitHub write is not allowed by the routing policy the command started under, or by the current one (not_governed); use a permitted operation, or ask the operator to update the policy and start the command again. This GitHub command is not a bot operation. If it should run as the operator, run it again with GH_SHIM_BYPASS=operator; the shim records that use.".to_string()]);
}

const UNDECLARED_POLICY_REFUSAL: &str = "gh-shim: gh_shim_seam_refusal: task_gh_policy_denied: this GitHub command is not declared in the signed manifest (not_governed); ask the operator to update the manifest and start the command again.";

#[test]
fn captured_policy_undeclared_command_does_not_claim_to_be_a_write() {
    let _lock = crate::test_env::process_env_lock();
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../../aft/tests/fixtures/gh_shim/v10-manifest.json"
    ))
    .unwrap();
    let _fixture = RunWithFixture::new(&manifest);
    let _provisioned = Env::set(
        crate::gh_shim_speech::start_view::PROVISIONED_ENV,
        Some(OsStr::new("1")),
    );
    for raw in [
        &["variable", "get", "REQUIRES_CORE", "-R", "cortexkit/aft"][..],
        &["run", "rerun", "123", "--failed", "--repo", "cortexkit/aft"][..],
    ] {
        let args = raw.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(matches!(
            classify(&args, &manifest, current_platform()),
            Classification::Unclassified
        ));
        let (_capture, lines) = capture_refusals_for_test();
        assert_eq!(
            run_with(
                &args,
                |_| panic!("an undeclared captured invocation must not reach native gh"),
                |_, _, _, _| panic!(
                    "an undeclared captured invocation must not reach the read probe"
                ),
            ),
            REFUSAL_EXIT_STATUS,
        );
        assert_eq!(
            *lines.lock().unwrap(),
            [UNDECLARED_POLICY_REFUSAL],
            "{raw:?}"
        );
    }
}

#[test]
fn provisioned_worker_run_rerun_never_suggests_the_operator_bypass() {
    let _lock = crate::test_env::process_env_lock();
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../../aft/tests/fixtures/gh_shim/v10-manifest.json"
    ))
    .unwrap();
    let _fixture = RunWithFixture::new(&manifest);
    let _provisioned = Env::set(
        crate::gh_shim_speech::start_view::PROVISIONED_ENV,
        Some(OsStr::new("1")),
    );
    let _worker = Env::set(
        crate::gh_shim_speech::start_view::WORKER_POLICY_ENV,
        Some(OsStr::new("1")),
    );
    let args: Vec<OsString> = ["run", "rerun", "123", "--failed", "--repo", "cortexkit/aft"]
        .into_iter()
        .map(Into::into)
        .collect();
    for bypass in [None, Some(OsStr::new("operator"))] {
        let _bypass = Env::set("GH_SHIM_BYPASS", bypass);
        let (_capture, lines) = capture_refusals_for_test();
        let exit = run_with(
            &args,
            |_| panic!("worker write delegated"),
            |_, _, _, _| panic!("worker refusal must not need a probe"),
        );
        assert_eq!(exit, REFUSAL_EXIT_STATUS);
        let lines = lines.lock().unwrap();
        assert_eq!(*lines, vec!["gh-shim: gh_shim_worker_write_refused: workers cannot write to GitHub; report what should be posted to your parent instead".to_string()]);
        assert!(!lines[0].contains("GH_SHIM_BYPASS"));
    }
}

fn operator_write_after_manifest_loss() -> (bool, Vec<String>) {
    let _lock = crate::test_env::process_env_lock();
    let manifest = super::tests::fixture_manifest();
    let fixture = RunWithFixture::new(&manifest);
    // Unsupported platforms classify using the checkout's Git origin. Give the
    // test an origin named in the signed manifest even in snapshots without .git.
    let checkout = tempfile::tempdir().unwrap();
    let git_dir = checkout.path().join(".git");
    let git_config = checkout.path().join("empty-gitconfig");
    std::fs::write(&git_config, "").unwrap();
    let _git_env = [
        Env::set("GIT_DIR", Some(git_dir.as_os_str())),
        Env::set("GIT_WORK_TREE", Some(checkout.path().as_os_str())),
        Env::set("GIT_COMMON_DIR", None),
        Env::set("GIT_CONFIG_COUNT", None),
        Env::set("GIT_CONFIG_GLOBAL", Some(git_config.as_os_str())),
        Env::set("GIT_CONFIG_NOSYSTEM", Some(OsStr::new("1"))),
    ];
    for args in [
        vec!["init", "--quiet"],
        vec![
            "remote",
            "add",
            "origin",
            "https://github.com/cortexkit/aft.git",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(checkout.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "git fixture failed: {output:?}");
    }
    assert_eq!(
        origin_remote(&std::env::current_dir().unwrap()).as_deref(),
        Some("https://github.com/cortexkit/aft.git")
    );
    let _bypass = Env::set("GH_SHIM_BYPASS", Some(OsStr::new("operator")));
    let args = [
        "issue",
        "comment",
        "7",
        "--repo",
        "outside/unbound",
        "--body",
        "body",
    ]
    .into_iter()
    .map(Into::into)
    .collect::<Vec<OsString>>();
    let (_capture, lines) = capture_refusals_for_test();
    let mut delegated = 0;
    let mut lookups = 0;
    let exit = run_with(
        &args,
        |_| {
            delegated += 1;
            71
        },
        |_, _, _, _| {
            lookups += 1;
            std::fs::remove_file(&fixture.paths.manifest).unwrap();
            Ok(None)
        },
    );
    assert_eq!(lookups, 1);
    assert_eq!(exit, REFUSAL_EXIT_STATUS);
    assert_eq!(delegated, 0);
    let lines = lines.lock().unwrap().clone();
    (fixture.paths.bypass_audit.is_file(), lines)
}

// Only the macOS and Linux manifest policy reaches the audited operator path.
#[cfg(not(windows))]
#[test]
fn audited_operator_write_rechecks_manifest_before_native_delegation() {
    let (audited, lines) = operator_write_after_manifest_loss();
    assert!(audited);
    assert_eq!(lines, ["gh-shim: gh_shim_manifest_unavailable: GitHub routing policy changed before the command could run. Start the command again."]);
}

#[cfg(windows)]
#[test]
fn windows_refuses_operator_write_when_manifest_disappears_without_auditing() {
    let (audited, lines) = operator_write_after_manifest_loss();
    assert!(!audited);
    assert_eq!(lines, ["gh-shim: gh_shim_manifest_unavailable: GitHub writes are refused because this machine's signed routing manifest is missing or invalid; without it the write would run as the operator's own account. Ask the operator to repair the manifest. Reads still work."]);
}

#[test]
fn post_probe_policy_arms_recheck_protection_and_refuse_provisioned_writes() {
    let _lock = crate::test_env::process_env_lock();
    for mode in ["dormant", "invalid", "regressed"] {
        for protected in [false, true] {
            let manifest = super::tests::fixture_manifest();
            let fixture = RunWithFixture::new(&manifest);
            // Accepting the initial manifest leaves durable provisioning records.
            // Losing the artifact during discovery cannot make this a public install.
            let repo = manifest.bindings.keys().next().unwrap();
            let args: Vec<OsString> = ["issue", "comment", "7", "--repo", repo, "--body", "body"]
                .into_iter()
                .map(Into::into)
                .collect();
            assert!(!crate::gh_shim_speech::start_view::protected(&args));
            let (_capture, lines) = capture_refusals_for_test();
            let reads = std::cell::Cell::new(0);
            let delegated = std::cell::Cell::new(0);
            let exit = run_with(
                &args,
                |_| {
                    delegated.set(delegated.get() + 1);
                    71
                },
                |_, _, _, _| {
                    reads.set(reads.get() + 1);
                    if protected {
                        unsafe {
                            std::env::set_var(
                                crate::gh_shim_speech::start_view::PROVISIONED_ENV,
                                "1",
                            )
                        };
                    }
                    if mode == "dormant" {
                        std::fs::remove_file(&fixture.paths.manifest).unwrap();
                    } else {
                        std::fs::write(&fixture.paths.manifest, b"invalid JSON").unwrap();
                        if mode == "invalid" {
                            std::fs::remove_file(&fixture.paths.last_valid_manifest).unwrap();
                        }
                    }
                    Ok(None)
                },
            );
            assert!(
                reads.get() > 0,
                "{mode}: production probe callback never ran"
            );
            assert!(
                matches!(
                    (mode, resolve_manifest(&fixture.paths, unix_seconds())),
                    ("dormant", ManifestResolution::Dormant)
                        | ("invalid", ManifestResolution::Invalid(_))
                        | ("regressed", ManifestResolution::Regressed { .. })
                ),
                "{mode}: wrong post-probe arm"
            );
            let expected = match (mode, protected) {
                ("regressed", true) => "gh-shim: gh_shim_seam_refusal: task_gh_authority_unavailable: this command's bot-write permission cannot be verified (current_manifest_unverifiable); start the command again from a live session, and tell the operator if it repeats.",
                // Where the last valid manifest governs nothing (Windows), the
                // command was never declared, so it refuses as unclassified.
                ("regressed", false) if current_platform() == "unsupported" => "gh-shim: gh_shim_unclassified: no manifest declaration for this invocation (manifest artifact fails validation)",
                ("regressed", false) => "gh-shim: gh_shim_manifest_regressed: the manifest artifact fails validation; issue comment is refused until the manifest is repaired",
                _ => "gh-shim: gh_shim_manifest_unavailable: GitHub writes are refused because this machine's signed routing manifest is missing or invalid; without it the write would run as the operator's own account. Ask the operator to repair the manifest. Reads still work.",
            };
            assert_eq!(exit, 86, "{mode}, protected={protected}");
            assert_eq!(delegated.get(), 0, "{mode}");
            assert_eq!(*lines.lock().unwrap(), vec![expected.to_string()], "{mode}");
        }
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn provisioned_admin_or_pr_labels_without_bypass_cannot_delegate() {
    let _lock = crate::test_env::process_env_lock();
    let workflow: Manifest = serde_json::from_str(include_str!(
        "../../../aft/tests/fixtures/gh_shim/v10-manifest.json"
    ))
    .unwrap();
    for (manifest, argv) in [
        (workflow, vec!["workflow", "run", "ci.yml", "--ref", "main"]),
        (
            super::tests::v14_manifest(),
            vec![
                "pr",
                "edit",
                "9",
                "--add-label",
                "trivial",
                "-R",
                "cortexkit/aft",
            ],
        ),
    ] {
        let _fixture = RunWithFixture::new(&manifest);
        let _provisioned = Env::set(
            crate::gh_shim_speech::start_view::PROVISIONED_ENV,
            Some(OsStr::new("1")),
        );
        let args: Vec<OsString> = argv.into_iter().map(Into::into).collect();
        assert!(matches!(
            classify(&args, &manifest, current_platform()),
            Classification::Admin { .. }
        ));
        let (_capture, lines) = capture_refusals_for_test();
        let delegated = std::cell::Cell::new(false);
        let exit = run_with(
            &args,
            |_| {
                delegated.set(true);
                0
            },
            |_, _, _, _| Ok(None),
        );
        assert_ne!(exit, 0);
        assert!(!delegated.get());
        assert_eq!(
            *lines.lock().unwrap(),
            vec![exact_policy_refusal("not_governed")]
        );
    }
}

#[test]
fn invalid_start_views_have_exact_run_with_refusals_without_delegation() {
    let _lock = crate::test_env::process_env_lock();
    for invalid in ["missing", "corrupt", "wrong-ticket"] {
        let manifest = super::tests::fixture_manifest();
        let fixture = RunWithFixture::new(&manifest);
        let _provisioned = Env::set(
            crate::gh_shim_speech::start_view::PROVISIONED_ENV,
            Some(OsStr::new("1")),
        );
        match invalid {
            "missing" => std::fs::remove_file(&fixture.view).unwrap(),
            "corrupt" => std::fs::write(&fixture.view, b"invalid JSON").unwrap(),
            "wrong-ticket" => std::fs::write(&fixture.view, serde_json::to_vec(&json!({
                "ticket_digest": crate::gh_shim_ticket::speech_digest("22222222222222222222222222222222").unwrap().to_vec(),
                "policy": snapshot(&manifest),
            })).unwrap()).unwrap(),
            _ => unreachable!(),
        }
        let repo = manifest.bindings.keys().next().unwrap();
        let args: Vec<OsString> = ["issue", "comment", "7", "--repo", repo, "--body", "body"]
            .into_iter()
            .map(Into::into)
            .collect();
        let (_capture, lines) = capture_refusals_for_test();
        let delegated = std::cell::Cell::new(false);
        let exit = run_with(
            &args,
            |_| {
                delegated.set(true);
                0
            },
            |_, _, _, _| Ok(None),
        );
        assert_ne!(exit, 0, "{invalid}");
        assert!(!delegated.get(), "{invalid}");
        // A platform the fixture manifest doesn't govern refuses before the
        // rung check, as in the preflight cases above.
        let expected = if current_platform() == "unsupported" {
            UNDECLARED_POLICY_REFUSAL.to_string()
        } else {
            exact_policy_refusal("not_r3")
        };
        assert_eq!(*lines.lock().unwrap(), vec![expected], "{invalid}");
    }
}
