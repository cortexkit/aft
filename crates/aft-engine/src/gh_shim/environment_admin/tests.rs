use super::*;
use crate::gh_shim::tests::{write_signed_manifest, ScopedTestEnvVar as Env};

const REPO: &str = "test-org/unbound";
const ENV: &str = "repos/test-org/unbound/environments/release";
const POLICY: &str = "repos/test-org/unbound/environments/release/deployment-branch-policies";
const SECRET: &[&str] = &["secret", "set", "SIGNING_KEY", "--env", "release"];

struct Fixture {
    paths: StatePaths,
    manifest: Manifest,
    _env: Vec<Env>,
    _home: tempfile::TempDir,
    _guard: crate::test_env::GhShimStateGuard,
}

impl Fixture {
    fn new(version: u64, declares_rows: bool) -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_str().unwrap();
        let mut env = vec![
            Env::set("GH_SHIM_BYPASS", Some("operator")),
            Env::set("GH_REPO", Some(REPO)),
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
            "AFT_GH_SPEECH_PROVISIONED",
            "AFT_GH_SPEECH_START_VIEW",
        ] {
            env.push(Env::set(key, None));
        }
        let paths = StatePaths::from_process();
        let mut manifest = v23_tests::manifest();
        manifest.manifest_version = version;
        manifest.bindings.clear();
        if declares_rows {
            let additions: Value = serde_json::from_str(include_str!(
                "../../../../aft/tests/fixtures/gh_shim/v30-additions.json"
            ))
            .unwrap();
            manifest.tiers.get_mut(&Tier::Admin).unwrap().extend(
                serde_json::from_value::<Vec<TupleDecl>>(additions["tiers"]["admin"].clone())
                    .unwrap(),
            );
            manifest.api_rules.extend(
                serde_json::from_value::<Vec<ApiRule>>(additions["api_rules"].clone()).unwrap(),
            );
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
        let status = run_with(&argv(raw), delegate, |_, _, _, _| Ok(None));
        let lines = lines.lock().unwrap().clone();
        (status, lines)
    }

    fn refuses(&self, args: &[&str], reason: &str) {
        let (status, lines) = self.run(args, |_| {
            panic!("a refused environment operation reached gh")
        });
        assert_eq!(status, REFUSAL_EXIT_STATUS, "{args:?}");
        assert!(
            matches!(lines.as_slice(), [line] if line.contains(reason)),
            "{args:?}: {lines:?}"
        );
        assert!(!self.paths.bypass_audit.exists());
    }
}

fn argv(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn environment_api_writes_preserve_stdin_or_fields_with_a_fixed_host_and_audit() {
    let fixture = Fixture::new(30, true);
    for (index, (raw, method, path, fields, tuple)) in [
        (
            vec!["api", "--method", "PUT", ENV, "--input", "-"],
            "PUT",
            ENV,
            vec!["--input=-"],
            "api:PUT:/repos/*/*/environments/*",
        ),
        (
            vec!["api", "-Fwait_timer=10", "-f", "foo=bar", "-XPUT", ENV],
            "PUT",
            ENV,
            vec!["--field=wait_timer=10", "--raw-field=foo=bar"],
            "api:PUT:/repos/*/*/environments/*",
        ),
        (
            vec!["api", "-X=POST", POLICY, "--input=-"],
            "POST",
            POLICY,
            vec!["--input=-"],
            "api:POST:/repos/*/*/environments/*/deployment-branch-policies",
        ),
        (
            vec!["api", POLICY, "-f", "name=v*", "-F", "type=tag"],
            "POST",
            POLICY,
            vec!["--raw-field=name=v*", "--field=type=tag"],
            "api:POST:/repos/*/*/environments/*/deployment-branch-policies",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, lines) = fixture.run(&raw, |forwarded| {
            let mut expected = vec!["api", "--method", method, path, "--hostname", "github.com"];
            expected.extend(fields.iter().copied());
            assert_eq!(forwarded, expected);
            let (records, error) = read_bypass_audit(&fixture.paths);
            assert!(error.is_none());
            let records = records.unwrap();
            assert_eq!(records.len(), index + 1);
            assert_eq!(records[index]["repository"], REPO);
            assert_eq!(records[index]["tuple"], tuple);
            23
        });
        assert_eq!((status, lines), (23, Vec::new()));
    }
}

#[test]
fn unbound_bypass_cannot_send_unreviewed_environment_writes_or_delete_environment_secrets() {
    let fixture = Fixture::new(30, true);
    for raw in [
        &["api", "-XDELETE", ENV][..],
        &[
            "api",
            "-XDELETE",
            "repos/test-org/unbound/%65nvironments/release/",
        ],
        &["api", "-iXDELETE", ENV],
        &["api", "-XGET", "-f", "foo=bar", ENV],
        &["api", "-XPUT", POLICY, "-f", "name=v*"],
        &[
            "api",
            "-XPOST",
            "repos/test-org/unbound/environments/release/secrets/KEY",
        ],
        &[
            "api",
            "-XDELETE",
            "repos/test-org/unbound/environments/release/deployment-branch-policies/7",
        ],
    ] {
        fixture.refuses(raw, "is not declared in the signed manifest");
    }
    for raw in [
        &["secret", "delete", "SIGNING_KEY", "--env", "release"][..],
        &["secret", "delete", "SIGNING_KEY", "-erelease"],
    ] {
        fixture.refuses(
            raw,
            "environment secret deletion is not declared in the signed manifest",
        );
    }
    let get = ["api", ENV, "-XGET", "-H", "Accept: application/json"];
    assert_eq!(
        fixture.run(&get, |forwarded| {
            assert_eq!(forwarded, get);
            23
        }),
        (23, Vec::new())
    );
    for args in [
        &["secret", "delete", "SIGNING_KEY"][..],
        &["secret", "delete", "SIGNING_KEY", "--org", "test-org"],
        &["secret", "delete", "SIGNING_KEY", "--user"],
    ] {
        assert_eq!(
            fixture.run(args, |forwarded| {
                assert_eq!(forwarded, args);
                23
            }),
            (23, Vec::new())
        );
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_previously_declared_environment_api_rule_keeps_its_existing_dispatch() {
    let mut fixture = Fixture::new(29, false);
    fixture.manifest.api_rules.push(ApiRule {
        method: "DELETE".into(),
        path_glob: PUT_GLOB.into(),
        tier: Tier::Admin,
        platform: vec!["macos".into(), "linux".into()],
        rationale: None,
    });
    write_signed_manifest(&fixture.paths, fixture.manifest.clone(), unix_seconds());
    let args = ["api", "-XDELETE", ENV];
    assert_eq!(
        fixture.run(&args, |forwarded| {
            assert_eq!(forwarded, args);
            23
        }),
        (23, Vec::new())
    );
}

#[test]
fn environment_api_rejects_mixed_bodies_files_argv_overrides_and_empty_payload() {
    let fixture = Fixture::new(30, true);
    for body in [
        &[][..],
        &["--input", "file.json"],
        &["--input=-", "-f", "name=v*"],
        &["-F", "name=@file"],
        &["-f", "name"],
        &["-f", "name=ok", "--hostname", "enterprise.example"],
        &["-f", "name=ok", "-XPUT"],
    ] {
        let mut args = vec!["api", "-XPUT", ENV];
        args.extend_from_slice(body);
        fixture.refuses(&args, "either --input - or -f/-F fields");
    }
}

#[test]
fn secret_set_rejects_every_value_flag_and_non_environment_scope_without_echoing_argv() {
    let fixture = Fixture::new(30, true);
    for raw in [
        &["secret", "set", "SIGNING_KEY"][..],
        &[
            "secret",
            "set",
            "SIGNING_KEY",
            "--env",
            "release",
            "--body",
            "SENSITIVE_ARG",
        ],
        &[
            "secret",
            "set",
            "SIGNING_KEY",
            "--env",
            "release",
            "-bSENSITIVE_ARG",
        ],
        &[
            "secret",
            "set",
            "SIGNING_KEY",
            "--env-file",
            "secrets.env",
            "--env",
            "release",
        ],
        &["secret", "set", "SIGNING_KEY", "--org", "test-org"],
        &["secret", "set", "SIGNING_KEY", "--user"],
        &[
            "secret",
            "set",
            "SIGNING_KEY",
            "--env",
            "release",
            "--app",
            "actions",
        ],
        &[
            "secret",
            "set",
            "SIGNING_KEY",
            "--env",
            "release",
            "--no-store",
        ],
        &["secret", "set", "SIGNING_KEY", "--env", "a/b"],
    ] {
        let (status, lines) = fixture.run(raw, |_| panic!("refused secret set reached gh"));
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert!(
            matches!(lines.as_slice(), [line] if line.contains("no other flags are supported") && !line.contains("SENSITIVE_ARG")),
            "{lines:?}"
        );
        assert!(!fixture.paths.bypass_audit.exists());
    }
}

#[test]
fn environment_admin_rows_are_refused_on_old_or_undeclared_manifests() {
    for (version, declares_rows) in [(29, true), (30, false)] {
        let fixture = Fixture::new(version, declares_rows);
        for args in [
            &["api", "-XPUT", ENV, "--input=-"][..],
            &["api", "-XPOST", POLICY, "-f", "name=v*"],
            SECRET,
        ] {
            assert!(matches!(
                super::super::classify(&argv(args), &fixture.manifest, "linux"),
                Classification::Unclassified
            ));
            fixture.refuses(args, &format!("is not declared in manifest {version}"));
        }
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn admin_bypass_is_required_and_terminal_secret_input_refuses_before_audit() {
    let fixture = Fixture::new(30, true);
    {
        let _env = Env::set("GH_SHIM_BYPASS", None);
        fixture.refuses(SECRET, "gh_shim_admin_tier");
        fixture.refuses(
            &["api", "-XPUT", ENV, "-Fwait_timer=0"],
            "gh_shim_admin_tier",
        );
        fixture.refuses(
            &["api", "-XPOST", POLICY, "-f", "name=v*"],
            "gh_shim_admin_tier",
        );
    }
    {
        let _env = Env::set("GH_TOKEN", Some("fake-bot-token"));
        fixture.refuses(SECRET, "gh_shim_bypass_bot_credentials");
        fixture.refuses(
            &["api", "-XPUT", ENV, "-Fwait_timer=0"],
            "gh_shim_bypass_bot_credentials",
        );
    }
    let (_guard, lines) = capture_refusals_for_test();
    assert_eq!(
        secret::dispatch(
            &argv(SECRET),
            &fixture.paths,
            Path::new("."),
            unix_seconds(),
            || false,
            |_| panic!("terminal input reached gh")
        ),
        REFUSAL_EXIT_STATUS
    );
    assert!(lines.lock().unwrap()[0].contains("stdin from a pipe or file"));
    assert!(!fixture.paths.bypass_audit.exists());
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn secret_set_resolves_the_repo_and_records_only_its_target_metadata() {
    let fixture = Fixture::new(30, true);
    for (args, repo) in [
        (SECRET.to_vec(), REPO),
        (
            vec![
                "secret",
                "set",
                "SIGNING_KEY",
                "--env=release",
                "-R",
                "test-org/explicit",
            ],
            "test-org/explicit",
        ),
        (
            vec![
                "secret",
                "set",
                "SIGNING_KEY",
                "-e",
                "release",
                "--repo=test-org/explicit",
            ],
            "test-org/explicit",
        ),
    ] {
        let code = secret::dispatch(
            &argv(&args),
            &fixture.paths,
            Path::new("."),
            unix_seconds(),
            || true,
            |forwarded| {
                assert_eq!(
                    forwarded,
                    [
                        "secret",
                        "set",
                        "SIGNING_KEY",
                        "--env",
                        "release",
                        "--repo",
                        &format!("github.com/{repo}")
                    ]
                );
                let (records, error) = read_bypass_audit(&fixture.paths);
                assert!(error.is_none());
                let records = records.unwrap();
                let record = records.last().unwrap();
                assert_eq!(
                    record,
                    &json!({ "as_of_unix_secs": record["as_of_unix_secs"], "tuple": "secret set", "repository": repo, "environment": "release", "secret_name": "SIGNING_KEY" })
                );
                23
            },
        );
        assert_eq!(code, 23);
    }
}
