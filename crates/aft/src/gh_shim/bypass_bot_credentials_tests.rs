use super::*;

use tests::ScopedTestEnvVar as Env;

struct Fixture {
    paths: StatePaths,
    home: tempfile::TempDir,
    manifest: Manifest,
    _env: Vec<Env>,
    _guard: crate::test_env::GhShimStateGuard,
}

impl Fixture {
    fn new() -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let home_text = home.path().to_str().unwrap();
        let mut env = Vec::new();
        for key in [
            "HOME",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
        ] {
            env.push(Env::set(key, Some(home_text)));
        }
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_CONFIG_DIR",
            "AFT_GH_SHIM_WORKER",
            "AFT_GH_READ_SOCKET",
            "AFT_GH_READ_CAPABILITY_FILE",
            "AFT_GH_SHIM_TICKET",
            "AFT_GH_WORKER_WRITE_POLICY",
            "AFT_GH_SPEECH_PROVISIONED",
            "AFT_GH_SPEECH_START_VIEW",
        ] {
            env.push(Env::set(key, None));
        }
        env.push(Env::set("GH_REPO", Some("cortexkit/aft")));
        env.push(Env::set("GH_SHIM_BYPASS", Some("operator")));
        let paths = StatePaths::from_process();
        let manifest = v23_tests::manifest();
        tests::write_signed_manifest(&paths, manifest.clone(), unix_seconds());
        Self {
            paths,
            home,
            manifest,
            _env: env,
            _guard: guard,
        }
    }

    fn dispatch(&self, args: &[OsString], delegate: impl FnOnce(&[OsString]) -> i32) -> i32 {
        dispatch_r3(
            args,
            classify(args, &self.manifest, "linux"),
            &self.manifest,
            &self.paths,
            &RungDetermination::r3(unix_seconds(), 23, &tests::test_rung_provenance()).record,
            &AgentBinding {
                repo: "cortexkit/aft".into(),
                agent_id: "alfonso-aft".into(),
            },
            unix_seconds(),
            delegate,
        )
    }
}

// Derive the inventory from the signed fixture so a newly declared admin row
// cannot silently escape the credential-refusal coverage.
fn admin_rows(manifest: &Manifest) -> Vec<(String, Vec<OsString>)> {
    let mut rows = Vec::new();
    for row in &manifest.tiers[&Tier::Admin] {
        let tuple = row.tuple();
        let mut raw: Vec<&str> = tuple.split_whitespace().collect();
        match tuple {
            "pr merge" => raw.extend(["7", "--squash"]),
            "label create" => raw.extend(["triage", "--color", "ff0000"]),
            "repo create" => raw.push("cortexkit/new-repo"),
            "repo rename" => raw.push("new-name"),
            "repo edit" => raw.extend(["--description", "updated"]),
            "release create" | "release edit" => raw.push("v1.0.0"),
            "release upload" => raw.extend(["v1.0.0", "artifact.zip"]),
            "workflow run" | "workflow enable" | "workflow disable" => raw.push("ci.yml"),
            "extension install" => raw.push("github/gh-stack"),
            "extension upgrade" | "extension remove" => raw.push("gh-stack"),
            _ if tuple.starts_with("stack ") => {}
            _ => raw.push("7"),
        }
        if tuple != "repo create" {
            raw.extend(["--repo", "cortexkit/aft"]);
        }
        rows.push((tuple.to_string(), raw.iter().map(OsString::from).collect()));
    }
    for rule in manifest
        .api_rules
        .iter()
        .filter(|rule| rule.tier == Tier::Admin)
    {
        let path = rule
            .path_glob
            .replacen('*', "cortexkit", 1)
            .replacen('*', "aft", 1)
            .replace('*', "7");
        rows.push((
            api_tuple(rule),
            vec![
                "api".into(),
                "-X".into(),
                rule.method.clone().into(),
                path.into(),
            ],
        ));
    }
    for (tuple, args) in &rows {
        assert!(
            matches!(classify(args, manifest, "linux"), Classification::Admin { tuple: found } if found == *tuple),
            "{tuple}"
        );
    }
    rows
}

// The full entry point classifies with this machine's platform, and the signed
// fixture declares some admin rows only for some platforms (on Windows,
// `pr merge` is undeclared and refused as unclassified before any bypass
// check). The direct dispatch below always classifies as Linux, so every row is
// still covered there; the full path checks only rows declared here.
fn declared_on_this_platform(fixture: &Fixture, args: &[OsString]) -> bool {
    matches!(
        classify(args, &fixture.manifest, current_platform()),
        Classification::Admin { .. }
    )
}

fn assert_bot_refusal(fixture: &Fixture, args: &[OsString], presence: &str, full_dispatch: bool) {
    if full_dispatch && !declared_on_this_platform(fixture, args) {
        return;
    }
    let (_sink, lines) = capture_refusals_for_test();
    let never_exec = |_: &[OsString]| panic!("bot credential bypass reached upstream: {args:?}");
    let status = if full_dispatch {
        run_with(args, never_exec, |_, _, _, _| Ok(None))
    } else {
        fixture.dispatch(args, never_exec)
    };
    assert_eq!(status, REFUSAL_EXIT_STATUS, "{args:?}");
    assert_eq!(*lines.lock().unwrap(), vec![format!("gh-shim: gh_shim_bypass_bot_credentials: {presence}; operator commands must run from a shell without bot credentials")]);
    assert!(!fixture.paths.bypass_audit.exists(), "{args:?}");
}

#[test]
fn every_admin_row_refuses_bot_environment_before_audit_or_exec() {
    let fixture = Fixture::new();
    let rows = admin_rows(&fixture.manifest);
    for name in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"] {
        // Even an empty token variable is ambient according to the shared detector.
        let _token = Env::set(name, Some(""));
        for (_, args) in &rows {
            for full_dispatch in [true, false] {
                assert_bot_refusal(&fixture, args, &format!("{name} is present"), full_dispatch);
            }
        }
    }
    let bot_config = fixture.home.path().join("elsewhere/gh-alfonso-test");
    fs::create_dir_all(&bot_config).unwrap();
    let _config = Env::set("GH_CONFIG_DIR", bot_config.to_str());
    for (_, args) in &rows {
        for full_dispatch in [true, false] {
            assert_bot_refusal(
                &fixture,
                args,
                "GH_CONFIG_DIR points to bot credentials",
                full_dispatch,
            );
        }
    }
}

#[test]
fn operator_environment_keeps_every_admin_bypass_audited() {
    let fixture = Fixture::new();
    let operator_config = fixture.home.path().join("gh");
    fs::create_dir_all(&operator_config).unwrap();
    let _config = Env::set("GH_CONFIG_DIR", operator_config.to_str());
    let _rename = Env::set("GH_SHIM_RENAME_PUBLISHED", Some("allow"));
    for (tuple, args) in admin_rows(&fixture.manifest) {
        let run = || {
            fixture.dispatch(&args, |forwarded| {
                assert_eq!(forwarded, args);
                let (audit, error) = read_bypass_audit(&fixture.paths);
                assert!(error.is_none());
                let audit = audit.unwrap();
                assert_eq!(audit.len(), 1);
                assert_eq!(audit[0]["tuple"], tuple);
                73
            })
        };
        let status = if tuple == "repo rename" {
            rename_guard::with_test_preflight_runner(
                |_| Err("test metadata unavailable".into()),
                run,
            )
            .0
        } else {
            run()
        };
        assert_eq!(status, 73, "{tuple}");
        fs::remove_file(&fixture.paths.bypass_audit).unwrap();
    }
}

#[test]
fn bot_credentials_also_refuse_label_and_queue_bypasses() {
    let fixture = Fixture::new();
    let _token = Env::set("GH_TOKEN", Some("bot-token"));
    for raw in [
        vec![
            "issue",
            "edit",
            "7",
            "--add-label",
            "triage",
            "--repo",
            "cortexkit/aft",
        ],
        vec![
            "pr",
            "edit",
            "7",
            "--add-label",
            "triage",
            "--repo",
            "cortexkit/aft",
        ],
        vec!["pr", "merge", "7", "--auto", "--repo", "cortexkit/aft"],
    ] {
        let args = raw.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(matches!(
            classify(&args, &fixture.manifest, "linux"),
            Classification::Governed { .. }
        ));
        for full_dispatch in [true, false] {
            assert_bot_refusal(&fixture, &args, "GH_TOKEN is present", full_dispatch);
        }
    }
    let args = vec!["auth".into(), "token".into()];
    assert_bot_refusal(&fixture, &args, "GH_TOKEN is present", true);
}

#[test]
fn bypass_reuses_manifest_detector_inventory_including_wrapper_directories() {
    let mut fixture = Fixture::new();
    let bot_config = fixture.home.path().join("gh-alfonso-test");
    fs::create_dir_all(&bot_config).unwrap();
    fixture.manifest.detectors.wrapper_config_dirs =
        vec![bot_config.to_string_lossy().into_owned()];
    let args = vec!["pr".into(), "merge".into(), "7".into()];
    assert_bot_refusal(
        &fixture,
        &args,
        &format!(
            "bot credential directory {} is present",
            bot_config.display()
        ),
        false,
    );

    fs::remove_dir(&bot_config).unwrap();
    let _custom = Env::set("AFT_TEST_BOT_CREDENTIAL", Some("present"));
    let detectors = Detectors {
        credential_env_names: vec!["AFT_TEST_BOT_CREDENTIAL".into()],
        wrapper_config_dirs: Vec::new(),
    };
    let (_sink, lines) = capture_refusals_for_test();
    assert_eq!(
        refuse_bypass_bot_credentials(&detectors),
        Some(REFUSAL_EXIT_STATUS)
    );
    assert_eq!(*lines.lock().unwrap(), vec!["gh-shim: gh_shim_bypass_bot_credentials: AFT_TEST_BOT_CREDENTIAL is present; operator commands must run from a shell without bot credentials"]);
}

#[test]
fn worker_admin_bypass_keeps_the_worker_write_refusal() {
    let fixture = Fixture::new();
    let socket = fixture.home.path().join("broker.sock");
    let capability = fixture.home.path().join("capability");
    let _worker = Env::set("AFT_GH_SHIM_WORKER", Some("1"));
    let _socket = Env::set("AFT_GH_READ_SOCKET", socket.to_str());
    let _capability = Env::set("AFT_GH_READ_CAPABILITY_FILE", capability.to_str());
    for token in [None, Some("bot-token")] {
        let _token = Env::set("GH_TOKEN", token);
        for (_, args) in admin_rows(&fixture.manifest) {
            let (_sink, lines) = capture_refusals_for_test();
            assert_eq!(
                run_with(
                    &args,
                    |_| panic!("worker bypass executed upstream"),
                    |_, _, _, _| panic!("worker bypass reached identity lookup")
                ),
                REFUSAL_EXIT_STATUS
            );
            assert_eq!(
                *lines.lock().unwrap(),
                vec![refusal_line(
                    RefusalCode::WorkerWriteRefused,
                    WORKER_WRITE_REFUSED_TEXT
                )]
            );
            assert!(!fixture.paths.bypass_audit.exists());
        }
    }
}
