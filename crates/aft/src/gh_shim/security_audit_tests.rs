use super::*;

struct Env(&'static str, Option<OsString>);
impl Env {
    fn set(key: &'static str, value: Option<&OsStr>) -> Self {
        let old = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self(key, old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

struct Fixture {
    paths: StatePaths,
    home: tempfile::TempDir,
    _env: Vec<Env>,
    _guard: crate::test_env::GhShimStateGuard,
}
impl Fixture {
    fn new(version: u64) -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let mut env = vec![
            Env::set("HOME", Some(home.path().as_os_str())),
            Env::set("USERPROFILE", Some(home.path().as_os_str())),
            Env::set("XDG_CONFIG_HOME", Some(home.path().as_os_str())),
            Env::set("GH_REPO", Some(OsStr::new("cortexkit/aft"))),
        ];
        for key in [
            "GH_SHIM_BYPASS",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_CONFIG_DIR",
            "GIT_CONFIG_COUNT",
        ] {
            env.push(Env::set(key, None));
        }
        let paths = StatePaths::from_process();
        let name = if version == 19 {
            include_str!("../../tests/fixtures/gh_shim/v19-manifest.json")
        } else {
            include_str!("../../tests/fixtures/gh_shim/v20-manifest.json")
        };
        let manifest: Manifest = serde_json::from_str(name).unwrap();
        tests::write_signed_manifest(&paths, manifest, unix_seconds());
        Self {
            paths,
            home,
            _env: env,
            _guard: guard,
        }
    }
    fn regress(&self) {
        assert!(matches!(
            resolve_manifest(&self.paths, unix_seconds()),
            ManifestResolution::Active(_)
        ));
        fs::write(&self.paths.manifest, b"not a signed envelope").unwrap();
    }
    fn repo(&self) -> PathBuf {
        let root = self.home.path().join("checkout");
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/cortexkit/aft.git",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        root
    }
}
fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

fn failure_flow(regressed: bool) {
    let fixture = Fixture::new(20);
    if regressed {
        fixture.regress();
    }
    for raw in [
        vec!["api", "-X", "POST", "repos/cortexkit/aft/issues"],
        vec!["api", "-f", "a=b", "repos/cortexkit/aft/issues"],
        vec!["api", "--input", "f", "repos/cortexkit/aft/issues"],
        vec!["api", "graphql"],
        vec!["issue", "create"],
        vec!["issue", "create", "--repo", "outside/unbound"],
        vec!["auth", "token"],
    ] {
        let mut lookups = 0;
        let status = run_with(
            &args(&raw),
            |_| panic!("legacy refusal must not invoke native gh: {raw:?}"),
            |_, _, _, _| {
                lookups += 1;
                Err("ticket_unknown".into())
            },
        );
        assert_eq!(status, 86, "{raw:?}");
        assert_eq!(lookups, 1, "the actual lookup-error boundary was exercised");
    }
    let raw = args(&["api", "repos/cortexkit/aft/issues", "--jq", ".[].title"]);
    let mut forwarded = Vec::new();
    let status = run_with(
        &raw,
        |args| {
            forwarded.push(args.to_vec());
            23
        },
        |_, _, _, _| Err("old daemon".into()),
    );
    assert_eq!(status, 23);
    assert_eq!(forwarded, vec![raw]);
}

#[test]
fn audit_lookup_error_resumes_all_active_v19_guards() {
    failure_flow(false);
}
#[test]
fn audit_lookup_error_resumes_regressed_v19_classification() {
    failure_flow(true);
}

#[test]
fn audit_repository_names_have_only_github_characters() {
    for repo in [
        "../user",
        "owner/..",
        "./repo",
        "?/repo",
        "owner/#",
        "%2e/repo",
        "owner/repo%2fmore",
        "ow ner/repo",
        "owner/répo",
    ] {
        assert!(canonical_repository_key(repo).is_none(), "{repo}");
        let request = read_contract::Request::Get {
            repository: repo.into(),
            args: read_contract::GetArgs {
                path: "issues".into(),
                query: BTreeMap::new(),
                accept: read_contract::Accept::Json,
            },
        };
        assert!(request.validate().is_err(), "{repo}");
    }
    assert_eq!(
        canonical_repository_key("Owner-1/Repo_2.name"),
        Some("owner-1/repo_2.name".into())
    );
}

#[test]
fn audit_daemon_resolves_verified_v20_checkout_not_ambient_repo() {
    let fixture = Fixture::new(20);
    let repo = fixture.repo();
    let snapshot = ticket_repository_snapshot(&repo);
    assert_eq!(snapshot.as_deref(), Some("cortexkit/aft"));
    let _foreign = Env::set("GH_REPO", Some(OsStr::new("cortexkit/common-auth")));
    assert_eq!(
        verified_worker_read_repository(snapshot.as_deref()).unwrap(),
        "cortexkit/aft"
    );
    fixture.regress();
    assert!(verified_worker_read_repository(snapshot.as_deref()).is_err());
}

#[test]
fn audit_daemon_rejects_v19_manifest_before_read_binding() {
    let fixture = Fixture::new(19);
    let repo = fixture.repo();
    let snapshot = ticket_repository_snapshot(&repo);
    let error = verified_worker_read_repository(snapshot.as_deref()).unwrap_err();
    assert!(error.contains("manifest v20"), "{error}");
}

#[test]
fn audit_v19_status_matches_legacy_bytes_even_with_a_lookup_record() {
    let fixture = Fixture::new(19);
    fs::write(
        fixture.paths.root.join("read-session-lookup.json"),
        br#"{"outcome":"unavailable","reason":"old daemon","at_unix_secs":7}"#,
    )
    .unwrap();
    let rendered = render_self_report(&fixture.paths).unwrap();
    let captured = include_str!("../../tests/fixtures/gh_shim/v19-main-status.jsonc");
    let (_, expected) = captured.split_once("// END HEADER\n").unwrap();
    let normalized = super::v19_compat_tests::normalize_v19_status(&rendered).unwrap();
    assert_eq!(
        normalized, expected,
        "v19 status must match the frozen origin/main binary capture"
    );
}

fn relay(ticket: Option<&str>) -> RelayContext {
    RelayContext {
        worker_broker: None,
        connection_file: None,
        ticket: ticket.map(str::to_string),
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(1),
    }
}

#[test]
fn audit_worker_native_exit_is_closed_for_every_legacy_command_shape() {
    for raw in [
        vec!["api", "--include", "repos/cortexkit/aft/issues"],
        vec![
            "api",
            "-X",
            "GET",
            "-X",
            "GET",
            "repos/cortexkit/aft/issues",
        ],
        vec!["api", "graphql", "-f", "query=query { viewer { login } }"],
        vec!["auth", "status"],
        vec!["repo", "clone", "other/repo"],
        vec!["--help"],
    ] {
        let status = native_guard::dispatch_with(
            &args(&raw),
            &relay(Some("worker-ticket")),
            native_guard::ShellSignals {
                sandboxed: true,
                worker_marked: false,
            },
            |_| Ok(Some(true)),
            |_| panic!("worker reached operator gh: {raw:?}"),
        );
        assert_eq!(status, 86);
    }
}

#[test]
fn audit_authenticated_head_keeps_native_argv_and_exit_status() {
    let argv = args(&["api", "graphql", "-f", "query=query { viewer { login } }"]);
    let mut calls = Vec::new();
    let status = native_guard::dispatch_with(
        &argv,
        &relay(Some("head-ticket")),
        native_guard::ShellSignals {
            sandboxed: false,
            worker_marked: false,
        },
        |_| Ok(Some(false)),
        |forwarded| {
            calls.push(forwarded.to_vec());
            27
        },
    );
    assert_eq!(status, 27);
    assert_eq!(calls, vec![argv]);
    assert!(native_guard::permission_with(
        &relay(None),
        native_guard::ShellSignals::default(),
        |_| panic!("ordinary head must not need a lookup")
    )
    .is_ok());
}

#[test]
fn review_unscoped_head_ticket_preserves_native_passthrough() {
    for outcome in [Ok(None), Err("prefrontal-core unavailable".into())] {
        let argv = args(&["pr", "view", "42"]);
        let mut calls = Vec::new();
        let status = native_guard::dispatch_with(
            &argv,
            &relay(Some("unscoped-head-ticket")),
            native_guard::ShellSignals::default(),
            |_| outcome,
            |forwarded| {
                calls.push(forwarded.to_vec());
                27
            },
        );
        assert_eq!(status, 27);
        assert_eq!(calls, vec![argv]);
    }
}

fn assert_worker_marker_refuses_native_gh(outcome: Result<Option<bool>, String>) {
    for ticket in [Some("worker-ticket"), None] {
        let permission = native_guard::permission_with(
            &relay(ticket),
            native_guard::ShellSignals {
                sandboxed: false,
                worker_marked: true,
            },
            |_| outcome.clone(),
        );
        assert_eq!(
            permission.unwrap_err(),
            "operator gh is not available to worker-marked shells; use governed reads or bot speech"
        );
    }
}

#[test]
fn review_worker_marker_refuses_native_gh_when_scope_lookup_errors() {
    assert_worker_marker_refuses_native_gh(Err("prefrontal-core unavailable".into()));
}

#[test]
fn review_worker_marker_refuses_native_gh_when_scope_is_empty() {
    assert_worker_marker_refuses_native_gh(Ok(None));
}

#[test]
fn review_worker_marker_cannot_be_overridden_by_head_attribution() {
    assert_worker_marker_refuses_native_gh(Ok(Some(false)));
}

#[test]
fn review_unscoped_head_keeps_v19_and_missing_manifest_reads() {
    for missing in [false, true] {
        let fixture = Fixture::new(19);
        if missing {
            fs::remove_file(&fixture.paths.manifest).unwrap();
        }
        let argv = args(&["pr", "view", "42"]);
        let status = run_with(
            &argv,
            |forwarded| {
                native_guard::dispatch_with(
                    forwarded,
                    &relay(Some("unscoped-head-ticket")),
                    native_guard::ShellSignals::default(),
                    |_| Ok(None),
                    |native_args| {
                        assert_eq!(native_args, argv);
                        27
                    },
                )
            },
            |_, _, _, _| Ok(None),
        );
        assert_eq!(status, 27, "missing manifest: {missing}");
    }
}

#[test]
fn review_canary_denial_overrides_missing_or_head_scope() {
    for outcome in [Ok(None), Ok(Some(false)), Err("no scope".into())] {
        let status = native_guard::dispatch_with(
            &args(&["pr", "view", "42"]),
            &relay(Some("ticket")),
            native_guard::ShellSignals {
                sandboxed: true,
                worker_marked: false,
            },
            |_| outcome,
            |_| panic!("jailed worker reached native gh"),
        );
        assert_eq!(status, 86);
    }
}

#[test]
#[cfg(not(target_os = "windows"))]
fn review_stamped_worker_reaches_governed_read_instead_of_native_gh() {
    use std::sync::Arc;
    let fixture = Fixture::new(20);
    let daemon = relay_client::tests::FakeRelayDaemon::spawn(Arc::new(|body| {
        let data = if body["op"] == crate::gh_shim_relay::SESSION_CONTEXT_OPERATION {
            json!({"worker":true,"head":false,"repository":"cortexkit/aft"})
        } else {
            assert_eq!(body["op"], crate::gh_shim_relay::READ_REQUEST_OPERATION);
            assert_eq!(body["params"]["request"]["repository"], "cortexkit/aft");
            assert_eq!(body["params"]["request"]["args"]["path"], "pulls/42");
            json!({"result":{"status":200,"body":{"number":42},"next_page":null},"isError":false})
        };
        json!({"op":body["op"],"status":"ok","data":data})
    }));
    let file = fixture.home.path().join("connection.json");
    daemon.write_connection_file(&file);
    let config = fixture.home.path().join("cortexkit/aft.jsonc");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        json!({"subc":{"connection_file":file}}).to_string(),
    )
    .unwrap();
    let _ticket = Env::set(
        crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
        Some(OsStr::new("worker-ticket")),
    );
    let status = run_with(
        &args(&["pr", "view", "42", "--json", "number"]),
        |_| panic!("stamped worker reached native gh"),
        worker_reads::identity_for_command,
    );
    assert_eq!(status, 0);
    let requests = daemon.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|body| body["op"] == crate::gh_shim_relay::READ_REQUEST_OPERATION)
            .count(),
        1
    );
}

#[test]
fn audit_unattributed_jailed_worker_never_gets_operator_gh() {
    for signals in [native_guard::ShellSignals {
        sandboxed: true,
        ..Default::default()
    }] {
        let status = native_guard::dispatch_with(
            &args(&["api", "repos/cortexkit/aft/issues"]),
            &relay(None),
            signals,
            |_| panic!("missing ticket cannot be looked up"),
            |_| panic!("stripped agent reached operator gh"),
        );
        assert_eq!(status, 86);
    }
    for outcome in [Err("old daemon".into()), Ok(None)] {
        assert!(native_guard::permission_with(
            &relay(Some("bad-ticket")),
            native_guard::ShellSignals {
                sandboxed: true,
                worker_marked: false
            },
            |_| outcome
        )
        .is_err());
    }
}

#[test]
fn audit_jailed_shell_without_ticket_names_the_worker_read_rule() {
    // `gh --version` or `gh api rate_limit` from a worker shell reaches this
    // refusal; the text must say what a worker can run, not blame a ticket.
    let error = native_guard::permission_with(
        &relay(None),
        native_guard::ShellSignals {
            sandboxed: true,
            worker_marked: false,
        },
        |_| panic!("a missing ticket cannot be looked up"),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "in a worker shell, gh runs only reads of a repository; this command is not one"
    );
}

/// Run the probe child in a fresh process, as a shell would run the shim:
/// `mode` picks what confines it, and `clear_all` mimics `env -i`.
///
/// Modes: `jail` confines the child the way AFT's worker jail does for the
/// canary (reads and writes of the canary directory denied). `outer-missing`
/// and `outer-readable` confine it the way a container or another tool's
/// sandbox does (Linux no_new_privs; macOS an unrelated Seatbelt profile), with
/// no canary or a readable one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_probe_child(mode: &str, clear_all: bool) -> (std::process::Output, tempfile::TempDir) {
    let bin = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let home = home.keep().canonicalize().unwrap();
    if mode != "outer-missing" {
        crate::worker_jail_canary::prepare(&home).unwrap();
    }
    fs::write(bin.path().join("probe-mode"), mode).unwrap();
    fs::write(
        bin.path().join("probe-home"),
        home.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let fake = bin.path().join("gh");
    crate::executable_cache::write_executable(
        &fake,
        b"#!/bin/sh\nprintf invoked > \"$0.ran\"\nprintf '123\\n'\nexit 0\n",
    )
    .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.current_dir(bin.path());
    command.args([
        "--exact",
        "gh_shim::security_audit_tests::stripped_worker_probe_child",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    if clear_all {
        command.env_clear();
    } else {
        command
            .env_remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV)
            .env_remove(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // Every Linux confinement in these modes sets no_new_privs, as an
        // outer container or CI sandbox does; only `jail` adds the canary deny.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let output = command.output().unwrap();
    let _ = fs::remove_dir_all(&home);
    (output, bin)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn audit_env_i_and_unset_ticket_keep_kernel_worker_fence() {
    let _lock = crate::test_env::process_env_lock();
    for clear_all in [false, true] {
        let (output, bin) = run_probe_child("jail", clear_all);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(!bin.path().join("gh.ran").exists());
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("gh_shim_worker_native_gh"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn audit_linux_prctl_error_still_checks_denied_worker_canary() {
    let _lock = crate::test_env::process_env_lock();
    let (output, _) = run_probe_child("jail-prctl-error", true);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A sandbox AFT did not create (a container's no_new_privs, a CI job sandbox,
/// another tool's Seatbelt profile) is not a worker jail: gh passes through.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn audit_outer_sandbox_without_the_worker_canary_keeps_native_passthrough() {
    let _lock = crate::test_env::process_env_lock();
    for mode in ["outer-missing", "outer-readable"] {
        for clear_all in [false, true] {
            let (output, bin) = run_probe_child(mode, clear_all);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{mode}: {stdout}\n{stderr}");
            // The shim execs the real gh in place of the probe process, so the
            // fake gh's output, not a test summary, ends the child's stdout.
            assert!(stdout.ends_with("123\n"), "{mode}: {stdout}");
            assert!(
                bin.path().join("gh.ran").exists(),
                "{mode}: native gh did not run"
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("gh_shim_worker_native_gh"),
                "{mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

/// Deny reads and writes of `denied` and nothing else, the way the worker jail
/// treats its canary directory. Landlock grants are allow-lists, so grant every
/// sibling along the path from `/` and leave only `denied` out.
#[cfg(target_os = "linux")]
fn confine_like_the_worker_jail(denied: &Path) {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };
    let abi = ABI::V3;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .unwrap()
        .create()
        .unwrap();
    let mut chain = denied.ancestors().collect::<Vec<_>>();
    chain.reverse();
    for pair in chain.windows(2) {
        for entry in fs::read_dir(pair[0]).unwrap() {
            let path = entry.unwrap().path();
            if path == pair[1] {
                continue;
            }
            let Ok(fd) = PathFd::new(&path) else { continue };
            let access = if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                AccessFs::from_all(abi)
            } else {
                AccessFs::from_file(abi)
            };
            ruleset = ruleset.add_rule(PathBeneath::new(fd, access)).unwrap();
        }
    }
    let status = ruleset.restrict_self().unwrap();
    assert_eq!(status.ruleset, RulesetStatus::FullyEnforced);
}

#[cfg(target_os = "macos")]
fn confine_with_seatbelt(source: &str) {
    unsafe extern "C" {
        fn sandbox_init(
            profile: *const libc::c_char,
            flags: u64,
            errorbuf: *mut *mut libc::c_char,
        ) -> libc::c_int;
    }
    let source = std::ffi::CString::new(source).unwrap();
    let mut error = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SBPL source and a writable error pointer.
    assert_eq!(unsafe { sandbox_init(source.as_ptr(), 0, &mut error) }, 0);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore]
fn stripped_worker_probe_child() {
    assert!(std::env::var_os(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV).is_none());
    assert!(std::env::var_os(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV).is_none());
    let cwd = std::env::current_dir().unwrap();
    let mode = fs::read_to_string(cwd.join("probe-mode")).unwrap();
    let home = PathBuf::from(String::from_utf8(fs::read(cwd.join("probe-home")).unwrap()).unwrap());
    let canary_dir = crate::worker_jail_canary::dir_for_home(&home);
    // Create the shim's state before confinement can deny new temp files.
    let _guard = crate::test_env::gh_shim_state_guard();
    std::env::set_var("PATH", &cwd);
    #[cfg(target_os = "linux")]
    if mode.starts_with("jail") {
        confine_like_the_worker_jail(&canary_dir);
    }
    #[cfg(target_os = "macos")]
    confine_with_seatbelt(&if mode == "jail" {
        format!(
            "(version 1)\n(allow default)\n(deny file-read* file-write* (subpath \"{}\"))\n",
            canary_dir.display()
        )
    } else {
        "(version 1)\n(allow default)\n(deny file-write* (subpath \"/aft-unrelated-outer-sandbox\"))\n"
            .to_string()
    });
    crate::worker_jail_canary::with_test_homes(vec![home], || {
        #[cfg(target_os = "linux")]
        if mode == "jail-prctl-error" {
            assert!(
                crate::worker_jail_canary::inside_worker_jail_with_no_new_privs(-1),
                "prctl error hid a denied canary"
            );
            assert!(!crate::worker_jail_canary::inside_worker_jail_with_no_new_privs(0));
        }
        let signals = native_guard::ShellSignals::from_process();
        // The probe's raw results (resolved canary path, stat errno, and on
        // macOS the sandbox_check return and errno) explain a wrong verdict.
        let diagnostics = crate::worker_jail_canary::diagnostics();
        if !mode.starts_with("jail") {
            assert!(!signals.sandboxed, "{mode}: {diagnostics}");
            assert!(native_guard::permission_from_process().is_ok());
            // Passthrough replaces this process with the real gh, so it runs last.
            run(&args(&["version"]));
            unreachable!("native gh passthrough returned instead of exec");
        }
        assert!(signals.sandboxed, "{mode}: {diagnostics}");
        let error = native_guard::permission_with(&RelayContext::from_process(), signals, |_| {
            panic!("no ticket")
        })
        .unwrap_err();
        assert_eq!(
            error,
            "in a worker shell, gh runs only reads of a repository; this command is not one"
        );
        let status = native_guard::dispatch_with(
            &args(&["api", "repos/cortexkit/aft/issues"]),
            &RelayContext::from_process(),
            signals,
            |_| panic!("no ticket"),
            |_| panic!("env -i reached native gh"),
        );
        assert_eq!(status, 86);
        assert!(resolve_and_cache_numeric_id(&StatePaths::from_process(), "test-user").is_none());
        assert!(rename_guard::resolve_preflight_gh().is_none());
        assert_eq!(run(&args(&["api", "repos/cortexkit/aft/issues"])), 86);
    });
}

#[test]
fn audit_repository_snapshot_survives_origin_rewrite_after_ticket_mint() {
    use crate::gh_shim_relay::{
        RelayCall, RelayTransport, RouteScopes, ScopeSelector, ScopeStamp, TransportError,
        READ_REQUEST_OPERATION,
    };
    use std::sync::Mutex;
    struct Transport(Mutex<Vec<Value>>);
    impl RelayTransport for Transport {
        async fn plexus(
            &self,
            _root: &str,
            _session: &str,
            _scope: Option<&ScopeSelector>,
            body: Value,
        ) -> Result<Value, TransportError> {
            self.0.lock().unwrap().push(body);
            Ok(json!({"isError":false,"result":{"status":200,"body":[],"next_page":null}}))
        }
        fn opens_scoped_routes(&self) -> bool {
            true
        }
    }
    let fixture = Fixture::new(20);
    let repo = fixture.repo();
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue(
        "snapshot-worker",
        "read-call",
        repo.to_str().unwrap(),
    );
    let value = ticket.value().unwrap();
    assert_eq!(
        crate::gh_shim_ticket::redeem(value)
            .unwrap()
            .read_repository
            .as_deref(),
        Some("cortexkit/aft")
    );
    assert!(Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "remote",
            "set-url",
            "origin",
            "https://github.com/cortexkit/common-auth.git"
        ])
        .status()
        .unwrap()
        .success());
    let mut scopes = RouteScopes::default();
    assert_eq!(
        crate::gh_shim_ticket::redeem(value)
            .unwrap()
            .read_repository
            .as_deref(),
        Some("cortexkit/aft")
    );
    scopes.push(
        "snapshot-worker",
        Some(
            ScopeStamp::new(
                subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                "worker",
                1,
                subc_protocol::scope::ScopeKind::Worker,
            )
            .with_owner_authorized(true),
        ),
    );
    let transport = Transport(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (repository, allowed) in [("cortexkit/common-auth", false), ("cortexkit/aft", true)] {
        let params = json!({"ticket":value,"request":{"action":"github.repo.get","repository":repository,"args":{"path":"issues"}}});
        let response = runtime.block_on(crate::gh_shim_relay::relay(
            &transport,
            RelayCall {
                operation: READ_REQUEST_OPERATION,
                params: &params,
                first_party: true,
                route_scopes: &scopes,
            },
            &|_| {},
        ));
        assert_eq!(response.ok, allowed, "{repository}: {:?}", response.data);
    }
    let calls = transport.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["arguments"]["repository"], "cortexkit/aft");
}

/// Worker scopes are read-only. This replaces the earlier policy that allowed
/// worker bot speech through the relay while only native gh was forbidden.
#[test]
fn worker_stamp_never_posts_even_without_a_jail_or_with_a_head_route_rebound() {
    struct NeverSend;
    impl crate::gh_shim_relay::RelayTransport for NeverSend {
        async fn plexus(
            &self,
            _root: &str,
            _session: &str,
            _scope: Option<&subc_protocol::scope::ScopeSelector>,
            _body: Value,
        ) -> Result<Value, crate::gh_shim_relay::TransportError> {
            panic!("worker write reached the provider")
        }
        fn opens_scoped_routes(&self) -> bool {
            true
        }
    }
    let worker = subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker-scope",
        7,
        subc_protocol::scope::ScopeKind::Worker,
    )
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(Some("agent-worker".into()))
            .with_delegates(true),
    )
    .with_owner_authorized(true);
    let pending = crate::bash_background::registry::with_scope_spawn(Some(worker.clone()), || {
        crate::gh_shim_ticket::PendingTicket::issue("worker-policy", "/does-not-exist")
    });
    let mut head = worker;
    head.kind = subc_protocol::scope::ScopeKind::Head;
    head.scope_epoch = 99;
    let mut scopes = crate::gh_shim_relay::RouteScopes::default();
    scopes.push("worker-policy", Some(head));
    let params = json!({"ticket":pending.value().unwrap(),"request_nonce":"worker-attempt","request":{"action":"issue comment","repository":"owner/repo","metadata":{"agent_id":"agent-worker"}}});
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = runtime.block_on(crate::gh_shim_relay::relay(
        &NeverSend,
        crate::gh_shim_relay::RelayCall {
            operation: crate::gh_shim_relay::BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &scopes,
        },
        &|_| {},
    ));
    assert!(!reply.ok);
    assert_eq!(
        reply.data["refusal_code"],
        crate::gh_shim_speech::WORKER_WRITE_CODE
    );
}

#[test]
fn audit_worker_marker_is_injected_from_daemon_jail_state_not_inherited() {
    let _lock = crate::test_env::process_env_lock();
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.github.shim = false;
    config.git.co_author = "off".into();
    let mut environment = std::collections::HashMap::new();
    crate::sandbox_spawn::with_worker_jail(true, || {
        crate::agent_child_env::inject(&config, root.path(), &mut environment, None)
    })
    .unwrap();
    assert_eq!(
        environment
            .get(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV)
            .map(String::as_str),
        Some("1")
    );
    crate::sandbox_spawn::with_worker_jail(false, || {
        crate::agent_child_env::inject(&config, root.path(), &mut environment, None)
    })
    .unwrap();
    assert!(!environment.contains_key(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV));
}

#[test]
fn audit_ticket_snapshot_uses_primary_config_not_worktree_override() {
    let fixture = Fixture::new(20);
    let primary = fixture.repo();
    let hooks = fixture.home.path().join("empty-hooks");
    fs::create_dir(&hooks).unwrap();
    assert!(Command::new("git")
        .arg("-C")
        .arg(&primary)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c"
        ])
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(["commit", "--allow-empty", "--quiet", "-m", "fixture"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&primary)
        .args(["config", "extensions.worktreeConfig", "true"])
        .status()
        .unwrap()
        .success());
    let linked = fixture.home.path().join("linked");
    assert!(Command::new("git")
        .arg("-C")
        .arg(&primary)
        .args(["worktree", "add", "--quiet", "--detach"])
        .arg(&linked)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&linked)
        .args([
            "config",
            "--worktree",
            "remote.origin.url",
            "https://github.com/cortexkit/common-auth.git"
        ])
        .status()
        .unwrap()
        .success());
    let effective = Command::new("git")
        .arg("-C")
        .arg(&linked)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(effective.stdout).unwrap().trim(),
        "https://github.com/cortexkit/common-auth.git"
    );
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue(
        "primary-snapshot-worker",
        "read",
        linked.to_str().unwrap(),
    );
    assert_eq!(
        crate::gh_shim_ticket::redeem(ticket.value().unwrap())
            .unwrap()
            .read_repository
            .as_deref(),
        Some("cortexkit/aft")
    );
}

#[cfg(unix)]
#[test]
fn audit_head_child_adapters_do_not_inherit_worker_markers() {
    let _lock = crate::test_env::process_env_lock();
    let _marker = Env::set(
        crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
        Some(OsStr::new("1")),
    );
    let mut child = Command::new("/bin/sh");
    child.args(["-c", "printf '%s' \"${AFT_GH_SHIM_WORKER-unset}\""]);
    crate::agent_child_env::apply_to_command(&mut child, &std::collections::HashMap::new());
    assert_eq!(child.output().unwrap().stdout, b"unset");
    let root = tempfile::tempdir().unwrap();
    let (pty, _) = crate::sandbox_spawn::pty_command_for_plan(
        &crate::sandbox_spawn::SpawnPlan::Unsandboxed,
        OsStr::new("/bin/sh"),
        &[],
        &root.path().join("task"),
        root.path(),
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(pty
        .iter_full_env_as_str()
        .all(|(key, _)| key != crate::gh_shim_ticket::GH_SHIM_WORKER_ENV));
}

/// A worker shell carries a read broker, never a write ticket. Every
/// write-class command from it must be refused with the report-to-parent line
/// and exit 86 before any identity lookup, delegation, or relay traffic.
#[test]
fn audit_worker_write_is_refused_before_any_broker_or_relay_traffic() {
    use std::sync::Arc;
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(20);
    // A live relay is configured so that any contact would be observed; the
    // worker refusal must happen before the shim reaches it.
    let daemon = relay_client::tests::FakeRelayDaemon::spawn(Arc::new(|body| {
        let operation = body["op"].as_str().unwrap_or("?");
        if operation == crate::gh_shim_relay::BINDINGS_READ_OPERATION {
            return json!({"op": operation, "status": "ok", "data": {"result": {
                "repo_binding_generation": 1,
                "bindings": [],
            }}});
        }
        json!({"op": operation, "status": "ok", "data": {
            "repo_binding_generation": 1,
            "result": {"status": "completed", "result": {"url": "https://github.com/cortexkit/aft/issues/7#issuecomment-1"}},
        }})
    }));
    let connection = fixture.home.path().join("connection.json");
    daemon.write_connection_file(&connection);
    let config = fixture.home.path().join("cortexkit/aft.jsonc");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        json!({"subc":{"connection_file":connection}}).to_string(),
    )
    .unwrap();
    // The worker shell: read-broker env present, no ticket.
    let _worker = Env::set(
        crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
        Some(OsStr::new("1")),
    );
    let socket = fixture.home.path().join("broker.sock");
    let capability = fixture.home.path().join("capability");
    fs::write(&capability, "0123456789abcdef0123456789abcdef").unwrap();
    let _socket_env = Env::set(
        crate::gh_shim_worker_broker::SOCKET_ENV,
        Some(socket.as_os_str()),
    );
    let _capability_env = Env::set(
        crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV,
        Some(capability.as_os_str()),
    );
    let _no_ticket = Env::set(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, None);
    let (_sink, lines) = capture_refusals_for_test();
    let context = relay_client::RelayContext::from_process();
    assert!(
        context.worker_broker.is_some(),
        "the test did not build a worker relay context"
    );
    assert!(context.ticket.is_none());
    assert!(context.connection_file.is_none());
    let mut delegated = Vec::new();
    let mut lookups = 0;
    for raw in [
        vec![
            "issue",
            "comment",
            "7",
            "--body",
            "hello",
            "--repo",
            "cortexkit/aft",
        ],
        vec!["pr", "review", "7", "--approve", "--repo", "cortexkit/aft"],
        vec![
            "issue",
            "edit",
            "7",
            "--add-label",
            "bug",
            "--repo",
            "cortexkit/aft",
        ],
        vec![
            "issue",
            "close",
            "7",
            "--reason",
            "completed",
            "--repo",
            "cortexkit/aft",
        ],
        vec!["pr", "close", "7", "--repo", "cortexkit/aft"],
        vec![
            "api",
            "-X",
            "POST",
            "repos/cortexkit/aft/issues/7/comments",
            "-f",
            "body=hello",
        ],
    ] {
        let status = run_with(
            &args(&raw),
            |forwarded| {
                delegated.push(forwarded.to_vec());
                0
            },
            |_, _, _, _| {
                lookups += 1;
                Ok(None)
            },
        );
        assert_eq!(status, 86, "{raw:?}");
    }
    assert!(
        delegated.is_empty(),
        "worker writes must never delegate: {delegated:?}"
    );
    assert_eq!(lookups, 0, "worker writes must not reach identity lookup");
    assert!(
        daemon.requests.lock().unwrap().is_empty(),
        "worker writes must not reach the relay"
    );
    // The refusal the agent actually sees: the exact report-to-parent line,
    // once per refused command.
    let lines = lines.lock().unwrap();
    assert_eq!(
        lines.len(),
        6,
        "every worker write must refuse loudly: {lines:?}"
    );
    for line in lines.iter() {
        assert_eq!(
            line,
            "gh-shim: gh_shim_worker_write_refused: workers cannot write to GitHub; report what should be posted to your parent instead"
        );
    }
}

/// The refusal above must not open a broker connection either. A bound
/// listener observes the socket: with the refusal first, no handshake arrives.
#[cfg(unix)]
#[test]
fn audit_worker_write_opens_no_broker_connection() {
    use std::os::unix::net::UnixListener;
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(20);
    let _worker = Env::set(
        crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
        Some(OsStr::new("1")),
    );
    let socket = fixture.home.path().join("broker.sock");
    let capability = fixture.home.path().join("capability");
    fs::write(&capability, "0123456789abcdef0123456789abcdef").unwrap();
    let _socket_env = Env::set(
        crate::gh_shim_worker_broker::SOCKET_ENV,
        Some(socket.as_os_str()),
    );
    let _capability_env = Env::set(
        crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV,
        Some(capability.as_os_str()),
    );
    let _no_ticket = Env::set(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, None);
    let (_sink, lines) = capture_refusals_for_test();
    let listener = UnixListener::bind(&socket).unwrap();
    let status = run_with(
        &args(&[
            "issue",
            "comment",
            "7",
            "--body",
            "hello",
            "--repo",
            "cortexkit/aft",
        ]),
        |_| panic!("worker write must not delegate"),
        |_, _, _, _| panic!("worker write must not reach identity lookup"),
    );
    assert_eq!(status, 86);
    assert_eq!(
        lines.lock().unwrap().as_slice(),
        ["gh-shim: gh_shim_worker_write_refused: workers cannot write to GitHub; report what should be posted to your parent instead"],
    );
    listener.set_nonblocking(true).unwrap();
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        connected => panic!("worker write opened a broker connection: {connected:?}"),
    }
}

/// A worker read still goes to the read broker: the new write-only refusal
/// must not divert it. A minimal fake broker answers the session context and
/// one read; the test asserts the shim spoke to it and the read succeeded.
#[cfg(unix)]
#[test]
fn audit_worker_read_still_reaches_the_broker() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(20);
    let _worker = Env::set(
        crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
        Some(OsStr::new("1")),
    );
    let socket = fixture.home.path().join("broker.sock");
    let capability = fixture.home.path().join("capability");
    fs::write(&capability, "0123456789abcdef0123456789abcdef").unwrap();
    let _socket_env = Env::set(
        crate::gh_shim_worker_broker::SOCKET_ENV,
        Some(socket.as_os_str()),
    );
    let _capability_env = Env::set(
        crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV,
        Some(capability.as_os_str()),
    );
    let _no_ticket = Env::set(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, None);
    let (_sink, refusals) = capture_refusals_for_test();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let served = seen.clone();
    let server = std::thread::spawn(move || {
        let listener = UnixListener::bind(&socket).unwrap();
        loop {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut size = [0u8; 4];
            if stream.read_exact(&mut size).is_err() {
                break;
            }
            let mut body = vec![0u8; u32::from_be_bytes(size) as usize];
            if stream.read_exact(&mut body).is_err() {
                break;
            }
            let request: Value = serde_json::from_slice(&body).unwrap();
            let operation = request["operation"]["op"].as_str().unwrap().to_string();
            let data = match operation.as_str() {
                "gh_shim.session_context" => {
                    json!({"worker": true, "head": false, "repository": "cortexkit/aft"})
                }
                "gh_shim.read_request" => json!({
                    "result": {"status": 200, "body": {"number": 42}, "next_page": null},
                    "isError": false,
                }),
                unexpected => panic!("worker read sent an unexpected broker op: {unexpected}"),
            };
            let reply = serde_json::to_vec(&json!({"status": "ok", "data": data})).unwrap();
            stream
                .write_all(&(reply.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&reply).unwrap();
            served.lock().unwrap().push(operation.clone());
            if operation == "gh_shim.read_request" {
                break;
            }
        }
    });
    let mut delegated = Vec::new();
    let status = run_with(
        &args(&[
            "pr",
            "view",
            "42",
            "--json",
            "number",
            "--repo",
            "cortexkit/aft",
        ]),
        |forwarded| {
            delegated.push(forwarded.to_vec());
            23
        },
        worker_reads::identity_for_command,
    );
    assert_eq!(status, 0);
    assert!(
        refusals.lock().unwrap().is_empty(),
        "a successful worker read must refuse nothing"
    );
    assert!(
        delegated.is_empty(),
        "a worker read must be brokered, not delegated: {delegated:?}"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            "gh_shim.session_context",
            "gh_shim.session_context",
            "gh_shim.read_request"
        ],
        "a worker read must authenticate its session and read through the broker"
    );
    server.join().unwrap();
}

/// A head-session write keeps its existing route: the worker-only refusal
/// must not catch it. The fake relay answers the bindings check and
/// completes the write, so the command succeeds without delegation.
#[test]
fn audit_head_write_still_routes_through_the_relay() {
    use std::sync::Arc;
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(20);
    let mut manifest_value: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    // The fixture manifests review only macOS and Linux, but the shim
    // classifies with current_platform(): anywhere else the exercised tuple
    // would read as undeclared and the write could never route. Extend the
    // exercised tuple to the running platform so run_with can reach the relay
    // on every platform this test runs on. Nothing else about the fixture
    // changes.
    if let Some(entries) = manifest_value
        .get_mut("tiers")
        .and_then(|tiers| tiers.get_mut("governed"))
        .and_then(Value::as_array_mut)
    {
        for entry in entries {
            if entry.get("tuple").and_then(Value::as_str) != Some("issue comment") {
                continue;
            }
            if let Some(platforms) = entry.get_mut("platform").and_then(Value::as_array_mut) {
                let current = Value::String(current_platform().to_string());
                if !platforms.contains(&current) {
                    platforms.push(current);
                }
            }
        }
    }
    let manifest: Manifest = serde_json::from_value(manifest_value).unwrap();
    tests::write_signed_manifest(&fixture.paths, manifest.clone(), unix_seconds());
    let bindings: Vec<Value> = manifest
        .bindings
        .iter()
        .map(
            |(repo, agent)| json!({"repository": repo, "agent_id": agent, "app_handle_id": "test"}),
        )
        .collect();
    let daemon = relay_client::tests::FakeRelayDaemon::spawn(Arc::new(move |body| {
        let operation = body["op"].as_str().unwrap_or("?");
        if operation == crate::gh_shim_relay::BINDINGS_READ_OPERATION {
            return json!({"op": operation, "status": "ok", "data": {"result": {
                "repo_binding_generation": 1,
                "bindings": bindings,
            }}});
        }
        assert_eq!(operation, crate::gh_shim_relay::BOT_REQUEST_OPERATION);
        json!({"op": operation, "status": "ok", "data": {
            "repo_binding_generation": 1,
            "result": {"status": "completed", "result": {"url": "https://github.com/cortexkit/aft/issues/7#issuecomment-1", "id": 1}},
        }})
    }));
    let connection = fixture.home.path().join("connection.json");
    daemon.write_connection_file(&connection);
    let config = fixture.home.path().join("cortexkit/aft.jsonc");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        json!({"subc":{"connection_file":connection}}).to_string(),
    )
    .unwrap();
    let _no_worker = Env::set(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV, None);
    let _no_socket = Env::set(crate::gh_shim_worker_broker::SOCKET_ENV, None);
    let _no_capability = Env::set(crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV, None);
    let _ticket = Env::set(
        crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
        Some(OsStr::new("head-ticket")),
    );
    let argv = args(&[
        "issue",
        "comment",
        "7",
        "--body",
        "hello",
        "--repo",
        "cortexkit/aft",
    ]);
    let mut delegated = Vec::new();
    let (_sink, refusals) = capture_refusals_for_test();
    // Seed a fresh cached R3 rung with the same provenance the shim computes,
    // so the test never depends on live rung discovery timing: on a loaded
    // runner the daemon probe can overrun the discovery budget (the test then
    // takes ~2 s and the write refuses as governance-unavailable, as seen in
    // Windows CI), while a cached fresh R3 record is reused with no probe at
    // all. The write still travels the live relay below, so the test keeps
    // proving the worker-only gate lets a head write through to the relay.
    let now = unix_seconds();
    let provenance = RungRecordProvenance::for_target(
        &TargetRepository::from_invocation(&argv),
        &std::env::current_dir().unwrap(),
    );
    write_rung_record_silently(
        &fixture.paths,
        &RungDetermination::r3(now, manifest.manifest_version, &provenance).record,
    );
    let status = run_with(
        &argv,
        |forwarded| {
            delegated.push(forwarded.to_vec());
            23
        },
        worker_reads::identity_for_command,
    );
    assert_eq!(
        status,
        0,
        "a head write was refused instead of routed: {:?}",
        refusals.lock().unwrap()
    );
    assert!(
        refusals.lock().unwrap().is_empty(),
        "a routed head write must refuse nothing"
    );
    assert!(
        delegated.is_empty(),
        "a head write must route, not delegate: {delegated:?}"
    );
    assert!(
        daemon
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["op"] == crate::gh_shim_relay::BOT_REQUEST_OPERATION),
        "a head write must still reach the relay"
    );
}

/// Credential commands are not writes: `gh auth token`, a token-revealing
/// status flag, and `gh auth login` from a worker shell must keep the
/// operator-credentials refusal (exit 86), never the worker-write line.
#[test]
fn audit_worker_credential_command_keeps_the_credential_refusal() {
    let _lock = crate::test_env::process_env_lock();
    // Held for its environment and installed manifest, not its paths: the
    // credential arm needs an active manifest, nothing else from the fixture.
    let _fixture = Fixture::new(20);
    let _worker = Env::set(
        crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
        Some(OsStr::new("1")),
    );
    let _no_ticket = Env::set(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, None);
    let _no_bypass = Env::set("GH_SHIM_BYPASS", None);
    let (_sink, lines) = capture_refusals_for_test();
    let mut lookups = 0;
    for raw in [
        vec!["auth", "token"],
        vec!["auth", "status", "--show-token"],
        vec!["auth", "login"],
    ] {
        let status = run_with(
            &args(&raw),
            |_| panic!("a credential refusal must not delegate: {raw:?}"),
            // The read-identity check runs before the credential arm and
            // declines credential shapes without any lookup; it must still
            // never report a worker session for them.
            |_, _, _, _| {
                lookups += 1;
                Ok(None)
            },
        );
        assert_eq!(status, 86, "{raw:?}");
    }
    assert_eq!(
        lookups, 3,
        "each credential shape consults read identity once"
    );
    let lines = lines.lock().unwrap();
    assert_eq!(
        lines.len(),
        3,
        "every credential command must refuse loudly: {lines:?}"
    );
    for line in lines.iter() {
        assert!(
            line.starts_with("gh-shim: gh_shim_operator_credentials: "),
            "a worker credential command must keep the credential refusal code: {line}"
        );
    }
}
