use super::*;
use crate::gh_shim::tests::{write_signed_manifest, ScopedTestEnvVar as Env};

const REPO: &str = "test-org/unbound";
const BODY: &str = "0.24.8";
const SET: &[&str] = &["variable", "set", "REQUIRES_CORE", "--body", BODY];
const LIST: &[&str] = &["variable", "list"];

struct Fixture {
    paths: StatePaths,
    manifest: Manifest,
    _env: Vec<Env>,
    _home: tempfile::TempDir,
    _guard: crate::test_env::GhShimStateGuard,
}

impl Fixture {
    fn new(version: u64, declare_rows: bool) -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_str().unwrap();
        let mut env = vec![
            Env::set("GH_REPO", Some(REPO)),
            Env::set("GH_HOST", Some("enterprise.example")),
            Env::set("AFT_GH_SPEECH_PROVISIONED", Some("1")),
        ];
        for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
            env.push(Env::set(key, Some(home_path)));
        }
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_CONFIG_DIR",
            "GH_SHIM_BYPASS",
            "AFT_GH_SHIM_WORKER",
            "AFT_GH_READ_SOCKET",
            "AFT_GH_READ_CAPABILITY_FILE",
            "AFT_GH_SHIM_TICKET",
            "AFT_GH_WORKER_WRITE_POLICY",
            "AFT_GH_SPEECH_START_VIEW",
        ] {
            env.push(Env::set(key, None));
        }
        let paths = StatePaths::from_process();
        let mut manifest = v23_tests::manifest();
        manifest.manifest_version = version;
        manifest.bindings.clear();
        if declare_rows {
            let additions: Value = serde_json::from_str(include_str!(
                "../../tests/fixtures/gh_shim/v30-additions.json"
            ))
            .unwrap();
            for (tier, key) in [(Tier::Admin, "admin"), (Tier::Mechanical, "mechanical")] {
                manifest.tiers.entry(tier).or_default().extend(
                    serde_json::from_value::<Vec<TupleDecl>>(additions["tiers"][key].clone())
                        .unwrap(),
                );
            }
        }
        write_signed_manifest(&paths, manifest.clone(), unix_seconds());
        Self {
            paths,
            manifest,
            _env: env,
            _home: home,
            _guard: guard,
        }
    }

    fn run(&self, raw: &[&str], delegate: impl FnMut(&[OsString]) -> i32) -> (i32, Vec<String>) {
        let (_guard, lines) = capture_refusals_for_test();
        let status = run_with(&argv(raw), delegate, |_, _, _, _| {
            panic!("variable operation reached the worker read probe")
        });
        let lines = lines.lock().unwrap().clone();
        (status, lines)
    }

    fn refuses(&self, args: &[&str], reason: &str) {
        let (status, lines) = self.run(args, |_| panic!("refused variable operation reached gh"));
        assert_eq!(status, REFUSAL_EXIT_STATUS, "{args:?}");
        assert!(
            matches!(lines.as_slice(), [line] if line.contains(reason)),
            "{args:?}: {lines:?}"
        );
        assert!(!self.paths.bypass_audit.exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn assert_last_audit(&self, repo: &str, environment: Option<&str>) {
        let (audit, error) = read_bypass_audit(&self.paths);
        assert!(error.is_none());
        let audit = audit.unwrap();
        let record = audit.last().unwrap();
        assert_eq!(
            record,
            &json!({
                "as_of_unix_secs": record["as_of_unix_secs"], "tuple": "variable set",
                "repository": repo, "environment": environment, "variable_name": "REQUIRES_CORE",
            })
        );
    }
}

fn argv(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn variable_set_body_and_stdin_forms_use_operator_identity_and_audit_target_only() {
    let fixture = Fixture::new(30, true);
    let _bypass = Env::set("GH_SHIM_BYPASS", Some("operator"));
    for (raw, repo, env) in [
        (SET.to_vec(), REPO, None),
        (
            vec![
                "variable",
                "set",
                "REQUIRES_CORE",
                "-b0.24.8",
                "--env=alpha",
                "-R",
                "test-org/explicit",
            ],
            "test-org/explicit",
            Some("alpha"),
        ),
    ] {
        assert!(
            matches!(super::super::classify(&argv(&raw), &fixture.manifest, "linux"), Classification::Admin { tuple } if tuple == "variable set")
        );
        assert_eq!(
            fixture.run(&raw, |forwarded| {
                let pinned_repo = format!("github.com/{repo}");
                let mut expected = vec!["variable", "set", "REQUIRES_CORE", "--repo", &pinned_repo];
                if let Some(env) = env {
                    expected.extend(["--env", env]);
                }
                expected.push("--body=0.24.8");
                assert_eq!(forwarded, expected);
                fixture.assert_last_audit(repo, env);
                23
            }),
            (23, Vec::new())
        );
    }
    let stdin_args = [
        "variable",
        "set",
        "REQUIRES_CORE",
        "--repo=test-org/explicit",
        "-e",
        "alpha",
    ];
    let status = dispatch_action(
        Action::Set,
        &argv(&stdin_args),
        &fixture.paths,
        || true,
        |forwarded| {
            assert_eq!(
                forwarded,
                [
                    "variable",
                    "set",
                    "REQUIRES_CORE",
                    "--repo",
                    "github.com/test-org/explicit",
                    "--env",
                    "alpha"
                ]
            );
            fixture.assert_last_audit("test-org/explicit", Some("alpha"));
            23
        },
    );
    assert_eq!(status, 23);
    assert!(!fs::read_to_string(&fixture.paths.bypass_audit)
        .unwrap()
        .contains(BODY));
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn variable_list_is_a_repo_or_environment_read_without_operator_audit() {
    let fixture = Fixture::new(30, true);
    for bypass in [None, Some("operator")] {
        let _env = Env::set("GH_SHIM_BYPASS", bypass);
        for (raw, repo, env) in [
            (LIST.to_vec(), REPO, None),
            (
                vec![
                    "variable",
                    "list",
                    "-R",
                    "test-org/explicit",
                    "--env",
                    "alpha",
                ],
                "test-org/explicit",
                Some("alpha"),
            ),
            (vec!["variable", "ls", "-ealpha"], REPO, Some("alpha")),
        ] {
            assert!(matches!(
                super::super::classify(&argv(&raw), &fixture.manifest, "linux"),
                Classification::Mechanical
            ));
            assert_eq!(
                fixture.run(&raw, |forwarded| {
                    let pinned_repo = format!("github.com/{repo}");
                    let mut expected = vec!["variable", "list", "--repo", &pinned_repo];
                    if let Some(env) = env {
                        expected.extend(["--env", env]);
                    }
                    assert_eq!(forwarded, expected);
                    assert!(!fixture.paths.bypass_audit.exists());
                    23
                }),
                (23, Vec::new())
            );
        }
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn variable_set_refuses_unreviewed_flags_without_recording_the_value() {
    let fixture = Fixture::new(30, true);
    let _bypass = Env::set("GH_SHIM_BYPASS", Some("operator"));
    for flags in [
        &["--org", "test-org"][..],
        &["--user"],
        &["--visibility=all"],
        &["--env-file", "vars.env"],
        &["-f-"],
        &["--repos", "repo"],
        &["--body="],
        &["--body", BODY, "-bduplicate"],
    ] {
        let mut raw = vec!["variable", "set", "REQUIRES_CORE"];
        raw.extend_from_slice(flags);
        fixture.refuses(&raw, "no other flags are supported");
    }
    fixture.refuses(
        &[
            "variable",
            "set",
            "REQUIRES_CORE",
            "--body",
            BODY,
            "-R",
            "enterprise.example/test-org/repo",
        ],
        "require a github.com repository",
    );
    let (_capture, lines) = capture_refusals_for_test();
    let status = dispatch_action(
        Action::Set,
        &argv(&["variable", "set", "REQUIRES_CORE"]),
        &fixture.paths,
        || false,
        |_| panic!("missing stdin reached gh"),
    );
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert!(lines.lock().unwrap()[0].contains("requires stdin from a pipe or file"));
    assert!(!fixture.paths.bypass_audit.exists());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn variable_list_refuses_non_repository_scopes_and_native_worker_access() {
    let fixture = Fixture::new(30, true);
    for (raw, message) in [
        (
            &["variable", "list", "--org", "test-org"][..],
            "accepts only a repository selector",
        ),
        (
            &["variable", "ls", "-otest-org"][..],
            "accepts only a repository selector",
        ),
        (
            &["variable", "list", "--user"][..],
            "accepts only a repository selector",
        ),
        (
            &["variable", "list", "--env", "../other"][..],
            "accepts only a repository selector",
        ),
        (
            &["variable", "list", "-R", "enterprise.example/test-org/repo"][..],
            "require a github.com repository",
        ),
    ] {
        fixture.refuses(raw, message);
    }
    let _worker = Env::set("AFT_GH_WORKER_WRITE_POLICY", Some("1"));
    fixture.refuses(LIST, "not available in worker shells");
}

#[test]
fn variable_delete_refuses_even_with_an_unbound_operator_bypass() {
    let fixture = Fixture::new(30, true);
    for bypass in [None, Some("operator")] {
        let _bypass = Env::set("GH_SHIM_BYPASS", bypass);
        for verb in ["delete", "remove", "rm"] {
            fixture.refuses(
                &["variable", verb, "REQUIRES_CORE", "-R", REPO],
                "variable delete is not supported",
            );
        }
    }
}

#[test]
fn variable_rows_require_the_signed_manifest_version_and_declarations() {
    for (version, declares_rows) in [(29, false), (29, true), (30, false)] {
        let fixture = Fixture::new(version, declares_rows);
        for (raw, bypass) in [(LIST, None), (SET, Some("operator"))] {
            let _bypass = Env::set("GH_SHIM_BYPASS", bypass);
            assert!(matches!(
                super::super::classify(&argv(raw), &fixture.manifest, "linux"),
                Classification::Unclassified
            ));
            fixture.refuses(raw, &format!("is not declared in manifest {version}"));
        }
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn variable_set_refuses_without_operator_bypass_or_with_bot_credentials() {
    let fixture = Fixture::new(30, true);
    fixture.refuses(SET, "gh_shim_admin_tier");
    let _bypass = Env::set("GH_SHIM_BYPASS", Some("operator"));
    let _bot = Env::set("GH_TOKEN", Some("fake-bot-token"));
    fixture.refuses(SET, "gh_shim_bypass_bot_credentials");
}
