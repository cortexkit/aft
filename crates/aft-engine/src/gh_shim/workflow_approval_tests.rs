use super::*;
use std::cell::RefCell;
use tests::ScopedTestEnvVar as Env;

#[cfg(not(windows))]
const REPO: &str = "test-org/unbound";
const ENDPOINT: &str = "repos/test-org/unbound/actions/runs/42/approve";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const HOSTED: &str =
    "jobs:\n  smoke:\n    runs-on: ubuntu-latest\n  other:\n    runs-on: ['windows-2025']\n";

struct Replies {
    run: Value,
    workflow: Result<&'static str, &'static str>,
}

thread_local! {
    static REPLIES: RefCell<Option<Replies>> = const { RefCell::new(None) };
}

fn fake_get(args: &[String]) -> Result<String, String> {
    REPLIES.with_borrow(|slot| {
        let replies = slot.as_ref().unwrap();
        if args.get(1).map(String::as_str) == Some("repos/test-org/unbound/actions/runs/42") {
            assert_eq!(
                args,
                [
                    "api",
                    "repos/test-org/unbound/actions/runs/42",
                    "--hostname",
                    "github.com"
                ]
            );
            Ok(replies.run.to_string())
        } else {
            assert_eq!(
                args,
                [
                    "api",
                    &format!(
                        "repos/fork-owner/unbound/contents/.github/workflows/ci.yml?ref={HEAD}"
                    ),
                    "--hostname",
                    "github.com",
                    "-H",
                    "Accept: application/vnd.github.raw+json",
                ]
            );
            replies.workflow.map(str::to_owned).map_err(str::to_owned)
        }
    })
}

fn replies(event: &str, status: &str, workflow: Result<&'static str, &'static str>) -> Replies {
    Replies {
        run: json!({
            "id": 42, "event": event, "status": status, "head_sha": HEAD,
            "path": ".github/workflows/ci.yml", "head_repository": { "full_name": "fork-owner/unbound" },
        }),
        workflow,
    }
}

struct Fixture {
    paths: StatePaths,
    _env: Vec<Env>,
    _home: tempfile::TempDir,
    _guard: crate::test_env::GhShimStateGuard,
}

impl Fixture {
    fn new(version: u64, declares_approval: bool) -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let home_text = home.path().to_str().unwrap();
        let mut env = vec![Env::set("GH_SHIM_BYPASS", Some("operator"))];
        for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
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
        let paths = StatePaths::from_process();
        let mut manifest = v23_tests::manifest();
        manifest.bindings.clear();
        manifest.manifest_version = version;
        if declares_approval {
            let additions: Value = serde_json::from_str(include_str!(
                "../../../aft/tests/fixtures/gh_shim/v30-additions.json"
            ))
            .unwrap();
            manifest.api_rules.extend(
                serde_json::from_value::<Vec<ApiRule>>(additions["api_rules"].clone()).unwrap(),
            );
        }
        tests::write_signed_manifest(&paths, manifest, unix_seconds());
        Self {
            paths,
            _env: env,
            _home: home,
            _guard: guard,
        }
    }

    fn run(
        &self,
        raw: &[&str],
        replies: Replies,
        delegate: impl FnMut(&[OsString]) -> i32,
    ) -> (i32, usize, Vec<String>) {
        REPLIES.set(Some(replies));
        let (_guard, refusals) = capture_refusals_for_test();
        let args = raw.iter().map(OsString::from).collect::<Vec<_>>();
        let (status, reads) = rename_guard::with_test_preflight_runner(fake_get, || {
            run_with(&args, delegate, |_, _, _, _| Ok(None))
        });
        REPLIES.set(None);
        let lines = refusals.lock().unwrap().clone();
        (status, reads, lines)
    }

    fn refuses(&self, raw: &[&str], replies: Replies, expected_reads: usize, reason: &str) {
        let (status, reads, lines) = self.run(raw, replies, |_| {
            panic!("a refused approval reached the native gh delegate")
        });
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(reads, expected_reads);
        assert!(
            matches!(lines.as_slice(), [line] if line.contains(reason)),
            "{lines:?}"
        );
        assert!(!self.paths.bypass_audit.exists());
    }
}

// Manifest version 30 declares fork-run approval for macOS and Linux only.
#[cfg(not(windows))]
#[test]
fn hosted_workflow_is_checked_at_the_fork_head_and_audited_before_post() {
    let fixture = Fixture::new(30, true);
    let events = [
        "pull_request",
        "pull_request_review",
        "pull_request_review_comment",
    ];
    for (index, event) in events.into_iter().enumerate() {
        let (status, reads, lines) = fixture.run(
            &["api", "--method", "POST", ENDPOINT],
            replies(event, "action_required", Ok(HOSTED)),
            |args| {
                assert_eq!(
                    args,
                    [
                        "api",
                        "--method",
                        "POST",
                        ENDPOINT,
                        "--hostname",
                        "github.com"
                    ]
                );
                let (audit, error) = read_bypass_audit(&fixture.paths);
                assert_eq!(error, None);
                let audit = audit.unwrap();
                assert_eq!(audit.len(), index + 1);
                assert_eq!(audit[index]["repository"], REPO);
                assert_eq!(
                    audit[index]["tuple"],
                    "api:POST:/repos/*/*/actions/runs/*/approve"
                );
                0
            },
        );
        assert_eq!((status, reads, lines), (0, 2, Vec::new()));
    }
}

#[cfg(not(windows))]
#[test]
fn unsafe_workflow_refuses_even_for_an_unbound_operator_bypass() {
    let fixture = Fixture::new(30, true);
    for (workflow, reason) in [
        (
            "jobs: {smoke: {runs-on: [ubuntu-latest, self-hosted]}}",
            "job \"smoke\" runs on \"self-hosted\"",
        ),
        (
            "jobs: {smoke: {runs-on: ubuntu-private}}",
            "job \"smoke\" uses unknown runner label \"ubuntu-private\"",
        ),
        (
            "jobs:\n  smoke:\n    runs-on: ${{ matrix.os }}\n",
            "job \"smoke\" uses an expression in runs-on",
        ),
        (
            "jobs: {smoke: {runs-on: {group: private, labels: ubuntu-latest}}}",
            "job \"smoke\" uses a runner group",
        ),
        (
            "jobs: {smoke: {uses: ./other.yml}}",
            "job \"smoke\" calls a reusable workflow",
        ),
        (
            "jobs: {smoke: {runs-on: [}",
            "could not inspect the workflow YAML",
        ),
        (
            "jobs: {smoke: {runs-on: ubuntu-latest, Runs-On: self-hosted}}",
            "could not inspect the workflow YAML",
        ),
    ] {
        fixture.refuses(
            &["api", "--method", "POST", ENDPOINT],
            replies("pull_request", "action_required", Ok(workflow)),
            2,
            reason,
        );
    }
}

#[cfg(not(windows))]
#[test]
fn missing_workflow_wrong_event_and_wrong_status_refuse_before_post() {
    let fixture = Fixture::new(30, true);
    for (upstream, reads, reason) in [
        (
            replies("pull_request", "action_required", Err("404")),
            2,
            "could not fetch the workflow file at the run's head commit",
        ),
        (
            replies("push", "action_required", Ok(HOSTED)),
            1,
            "run event \"push\" is not a fork pull request event",
        ),
        (
            replies("pull_request", "queued", Ok(HOSTED)),
            1,
            "run status \"queued\" is not \"action_required\"",
        ),
    ] {
        fixture.refuses(
            &["api", "--method", "POST", ENDPOINT],
            upstream,
            reads,
            reason,
        );
    }
}

#[test]
fn older_or_undeclared_manifest_refuses_before_reads() {
    for (version, declares_approval) in [(29, false), (29, true), (30, false)] {
        Fixture::new(version, declares_approval).refuses(
            &["api", "--method", "POST", ENDPOINT],
            replies("pull_request", "action_required", Ok(HOSTED)),
            0,
            &format!("is not declared in manifest {version}"),
        );
    }
}

#[test]
fn approval_cannot_override_the_checked_endpoint_or_host() {
    let fixture = Fixture::new(30, true);
    for raw in [
        vec![
            "api",
            "-XPOST",
            ENDPOINT,
            "--hostname",
            "enterprise.example",
        ],
        vec!["api", "-XPOST", ENDPOINT, "--input", "body.json"],
        vec![
            "api",
            "-XPOST",
            "repos/test-org/unbound/actions/runs/42/%61pprove?query=yes",
        ],
        vec![
            "api",
            "-XPOST",
            "repos/{owner}/{repo}/actions/runs/42/approve",
        ],
    ] {
        fixture.refuses(
            &raw,
            replies("pull_request", "action_required", Ok(HOSTED)),
            0,
            "with an explicit repository and no other flags",
        );
    }
}

#[cfg(not(windows))]
#[test]
fn approval_requires_the_operator_bypass_and_no_bot_credentials() {
    let fixture = Fixture::new(30, true);
    let raw = ["api", "-XPOST", ENDPOINT];
    {
        let _bypass = Env::set("GH_SHIM_BYPASS", None);
        fixture.refuses(
            &raw,
            replies("pull_request", "action_required", Ok(HOSTED)),
            0,
            "gh_shim_admin_tier",
        );
    }
    let _bot = Env::set("GH_TOKEN", Some("fake-bot-token"));
    fixture.refuses(
        &raw,
        replies("pull_request", "action_required", Ok(HOSTED)),
        0,
        "gh_shim_bypass_bot_credentials",
    );
}

#[cfg(windows)]
#[test]
fn windows_refuses_fork_run_approval_as_unclassified_before_preflight() {
    let fixture = Fixture::new(30, true);
    fixture.refuses(
        &["api", "-XPOST", ENDPOINT],
        replies("pull_request", "action_required", Ok(HOSTED)),
        0,
        "gh_shim_unclassified: api POST /repos/test-org/unbound/actions/runs/42/approve is not declared in manifest 30",
    );
}
