use super::*;
use crate::gh_shim::tests::{write_signed_manifest, ScopedTestEnvVar as Env};

struct Fixture {
    paths: StatePaths,
    _env: Vec<Env>,
    _home: tempfile::TempDir,
    _guard: crate::test_env::GhShimStateGuard,
}

fn manifest(version: u64, declared: bool) -> Manifest {
    let mut manifest = v23_tests::manifest();
    manifest.manifest_version = version;
    if declared {
        let additions: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/gh_shim/v30-additions.json"
        ))
        .unwrap();
        manifest.tiers.get_mut(&Tier::Admin).unwrap().extend(
            serde_json::from_value::<Vec<TupleDecl>>(additions["tiers"]["admin"].clone()).unwrap(),
        );
    }
    manifest
}

impl Fixture {
    fn new(version: u64, declared: bool) -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_str().unwrap();
        let mut env = vec![
            Env::set("GH_SHIM_BYPASS", Some("operator")),
            Env::set("GH_REPO", Some("test-org/current")),
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
        write_signed_manifest(&paths, manifest(version, declared), unix_seconds());
        Self {
            paths,
            _env: env,
            _home: home,
            _guard: guard,
        }
    }

    fn run(&self, args: &[&str], delegate: impl FnMut(&[OsString]) -> i32) -> (i32, Vec<String>) {
        let (_guard, lines) = capture_refusals_for_test();
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        let status = run_with(&args, delegate, |_, _, _, _| {
            panic!("secret list reached the broker read probe")
        });
        let lines = lines.lock().unwrap().clone();
        (status, lines)
    }

    fn refuses(&self, args: &[&str], expected: &str) {
        let (status, lines) = self.run(args, |_| panic!("refused secret list reached native gh"));
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(lines, [expected], "{args:?}");
        assert!(!self.paths.bypass_audit.exists());
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn repo_secret_list_is_scoped_and_audited_before_native_read() {
    let fixture = Fixture::new(30, true);
    for (index, (args, repo, expected_flags)) in [
        (vec!["secret", "list"], "test-org/current", vec![]),
        (
            vec!["secret", "list", "-R", "test-org/other"],
            "test-org/other",
            vec![],
        ),
        (
            vec![
                "secret",
                "list",
                "--app",
                "actions",
                "--json",
                "name,updatedAt",
            ],
            "test-org/current",
            vec!["--app", "actions", "--json", "name,updatedAt"],
        ),
        (
            vec!["secret", "list", "--app=dependabot", "--json=updatedAt"],
            "test-org/current",
            vec!["--app", "dependabot", "--json", "updatedAt"],
        ),
        (
            vec![
                "secret",
                "list",
                "--repo=test-org/other",
                "--app=codespaces",
                "--json",
                "name",
            ],
            "test-org/other",
            vec!["--app", "codespaces", "--json", "name"],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, lines) = fixture.run(&args, |forwarded| {
            let host_repo = format!("github.com/{repo}");
            let mut expected = vec!["secret", "list", "--repo", &host_repo];
            expected.extend_from_slice(&expected_flags);
            assert_eq!(forwarded, expected);
            let (audit, error) = read_bypass_audit(&fixture.paths);
            assert!(error.is_none(), "{error:?}");
            let audit = audit.unwrap();
            assert_eq!(audit.len(), index + 1);
            assert_eq!(audit[index]["repository"], repo);
            assert_eq!(audit[index]["tuple"], "secret list");
            0
        });
        assert_eq!(status, 0, "{args:?}: {lines:?}");
        assert!(lines.is_empty());
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn other_scopes_apps_and_json_fields_refuse_before_audit_and_native_read() {
    let fixture = Fixture::new(30, true);
    for scope in [
        vec!["--org", "test-org"],
        vec!["--user"],
        vec!["--env=production"],
        vec!["-otest-org"],
        vec!["-u"],
        vec!["-eproduction"],
    ] {
        let mut args = vec!["secret", "list", "-R", "test-org/other"];
        args.extend(scope);
        fixture.refuses(&args, "gh-shim: gh_shim_unsupported_flag: secret list is limited to repository scope; --org, --user, and --env are not supported");
    }
    for flags in [
        vec!["--app", "agents"],
        vec!["--json", "value"],
        vec!["--json", "visibility"],
        vec!["--json", "createdAt"],
        vec!["--json"],
        vec!["--jq", ".[]"],
    ] {
        let mut args = vec!["secret", "list"];
        args.extend(flags);
        fixture.refuses(&args, "gh-shim: gh_shim_unsupported_flag: secret list accepts only -R OWNER/REPO, --app actions|dependabot|codespaces, and --json name,updatedAt");
    }
    fixture.refuses(&["secret", "list", "-R", "enterprise.example/test-org/other"], "gh-shim: gh_shim_unbound_target: secret list requires a github.com repository; use -R OWNER/REPO or run from a checkout with a github.com origin");
}

#[test]
fn old_and_undeclared_manifests_name_the_read_with_or_without_bypass() {
    for (version, declared) in [(29, false), (29, true), (30, false)] {
        let fixture = Fixture::new(version, declared);
        for bypass in [None, Some("operator")] {
            let _bypass = Env::set("GH_SHIM_BYPASS", bypass);
            fixture.refuses(&["secret", "list", "-R", "test-org/other"], &format!("gh-shim: gh_shim_unclassified: read verb \"secret list\" is not declared in manifest {version}; ask the operator to update the manifest."));
        }
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn v30_secret_list_is_operator_only_and_leaves_other_secret_verbs_unclassified() {
    let fixture = Fixture::new(30, true);
    let args = ["secret", "list"];
    {
        let _bypass = Env::set("GH_SHIM_BYPASS", None);
        fixture.refuses(&args, "gh-shim: gh_shim_admin_tier: `secret list` is administration-tier — it runs under the operator's identity, not the bot's. Re-run with GH_SHIM_BYPASS=operator; the shim records an operator-attributed audit line.");
    }
    {
        let _worker = Env::set("AFT_GH_WORKER_WRITE_POLICY", Some("1"));
        fixture.refuses(&args, "gh-shim: gh_shim_worker_native_gh: repository secret metadata is available only to the operator, not to workers");
    }
    let manifest = manifest(30, true);
    for subcommand in ["set", "delete", "get"] {
        assert!(matches!(
            super::super::classify(
                &["secret", subcommand].map(OsString::from),
                &manifest,
                "linux"
            ),
            Classification::Unclassified
        ));
    }
}
