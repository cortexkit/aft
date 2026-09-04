#![cfg(unix)]

use std::env;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::outline::handle_outline;
use aft::commands::read::handle_read;
use aft::commands::zoom::handle_zoom;
use aft::config::{Config, GhReadConfig};
use aft::context::{default_language_provider_factory, AppContext};
use aft::github_read::{
    sqlite_cache_store, DownloadedGithubImage, GithubDocument, GithubDocumentKind,
    GithubFetchRequest, GithubFetcher, GithubImageDownloader, GithubReadClock,
    GithubReadCompletion, GithubReadEngine, GithubReadError, GithubReadSelector, GithubReadStart,
};
use aft::harness::Harness;
use aft::protocol::RawRequest;
use serde_json::json;
use url::Url;

use super::helpers::{AftProcess, ReleaseOnDrop};

const RESOURCE: &str = "issue://owner/repo/7";
const AMBIENT_OPERATOR_CREDENTIAL: &str = "ghp_operator_ambient_credential";
const AUTHORISATION_IDENTITY: &str = "Authorization: Bearer ghp_operator_ambient_credential";
const RESTRICTION_PROBE_CASE: &str = "AFT_GITHUB_RESTRICTION_PROBE_CASE";
const RESTRICTION_PROBE_PROJECT: &str = "AFT_GITHUB_RESTRICTION_PROBE_PROJECT";
const RESTRICTION_PROBE_DONE: &str = "AFT_GITHUB_RESTRICTION_PROBE_DONE";
const RESTRICTION_GH_LOG: &str = "AFT_GITHUB_RESTRICTION_GH_LOG";

fn enabled_gh_read() -> GhReadConfig {
    GhReadConfig { enabled: true }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write fixture executable");
    let mut permissions = fs::metadata(path)
        .expect("stat fixture executable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fixture executable");
}

fn write_success_gh(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).expect("create fixture gh directory");
    write_executable(
        &bin_dir.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$AFT_GH_READ_CALL_LOG"
printf '%s\n' '{"number":7,"title":"First-party fixture","state":"OPEN","body":"visible GitHub document","url":"https://github.com/owner/repo/issues/7","comments":[]}'
"#,
    );
}

fn write_failure_gh(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).expect("create failing gh directory");
    write_executable(
        &bin_dir.join("gh"),
        r#"#!/bin/sh
case "$3" in
  404)
    printf '%s\n' 'HTTP 404: private resource Authorization: Bearer ghp_operator_ambient_credential' >&2
    ;;
  405)
    printf '%s\n' 'HTTP 404: resource does not exist token=github_pat_operator_ambient_credential' >&2
    ;;
  401)
    printf '%s\n' 'HTTP 401: Bad credentials Authorization: Bearer ghp_operator_ambient_credential' >&2
    ;;
  *)
    printf '%s\n' 'unexpected fixture resource' >&2
    ;;
esac
exit 1
"#,
    );
}

fn path_with(bin_dir: &Path) -> std::ffi::OsString {
    let original_path = env::var_os("PATH").unwrap_or_default();
    env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(env::split_paths(&original_path)))
        .expect("join fixture PATH")
}

fn configure_gh_read(aft: &mut AftProcess, project: &Path, harness: &str) {
    // gh_read.enabled is USER-tier only (project tiers drop it), so tests
    // enable through an injected user config file.
    let user_config = project.join("user-aft.jsonc");
    fs::write(&user_config, r#"{"gh_read":{"enabled":true}}"#)
        .expect("write enabled GitHub-read user config");
    let configured = aft.send(
        &json!({
            "id": format!("configure-{harness}"),
            "command": "configure",
            "harness": harness,
            "project_root": project,
            "cortexkit_user_config_path": user_config,
        })
        .to_string(),
    );
    assert_eq!(
        configured["success"], true,
        "configure {harness} failed: {configured:#}"
    );
}

fn read_request(aft: &mut AftProcess, id: &str, resource: &str) -> serde_json::Value {
    aft.send_with_timeout(
        &json!({
            "id": id,
            "command": "read",
            "file": resource,
        })
        .to_string(),
        // Correctness budget, not a latency assertion: the deferred dispatch
        // path polls at 100ms and the fixture gh script's first exec pays the
        // macOS fresh-inode assessment tax, so a tight budget flakes under
        // load (rule 6987 class). Non-blocking contracts are asserted by the
        // dedicated slow-fetch sibling test, not here.
        Duration::from_secs(8),
    )
}

fn assert_no_raw_credentials(rendered: &str) {
    for forbidden in [
        AMBIENT_OPERATOR_CREDENTIAL,
        "github_pat_operator_ambient_credential",
        "Authorization: Bearer",
        "Bearer ghp_",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "GitHub read exposed sensitive material {forbidden:?}: {rendered}"
        );
    }
}

fn assert_no_sensitive_material(rendered: &str) {
    assert_no_raw_credentials(rendered);
    assert!(
        !rendered.contains("authentication_identity_hash"),
        "GitHub read exposed an authentication hash: {rendered}"
    );
}

#[test]
fn first_party_opencode_pi_and_runner_binds_remain_permitted() {
    let fixture = tempfile::tempdir().expect("create first-party fixture root");
    let bin_dir = fixture.path().join("bin");
    let call_log = fixture.path().join("gh-calls.log");
    write_success_gh(&bin_dir);
    fs::write(&call_log, "").expect("create gh call log");
    let path = path_with(&bin_dir);

    for harness in ["opencode", "pi", "runner"] {
        let project = tempfile::tempdir().expect("create first-party project");
        let mut aft = AftProcess::spawn_with_env(&[
            ("PATH", path.as_os_str()),
            ("AFT_GH_READ_CALL_LOG", call_log.as_os_str()),
        ]);
        configure_gh_read(&mut aft, project.path(), harness);

        let response = read_request(&mut aft, &format!("{harness}-github-read"), RESOURCE);
        assert_eq!(
            response["success"], true,
            "first-party {harness} bind should permit normal GitHub reads: {response:#}"
        );
        assert!(
            response["content"]
                .as_str()
                .unwrap_or_default()
                .contains("First-party fixture"),
            "first-party {harness} response must contain the fetched document: {response:#}"
        );
        assert_eq!(response["attachments"], json!([]));
        assert_no_sensitive_material(&response.to_string());
        assert!(aft.shutdown().success());
    }

    assert_eq!(
        fs::read_to_string(&call_log)
            .expect("read first-party gh call log")
            .lines()
            .count(),
        6,
        "each first-party bind must fetch the resource and its timeline through the GitHub CLI"
    );
}

#[test]
fn isolated_restriction_probe() {
    let Ok(case) = env::var(RESTRICTION_PROBE_CASE) else {
        return;
    };
    let project =
        PathBuf::from(env::var(RESTRICTION_PROBE_PROJECT).expect("restriction probe project path"));
    let completion_marker = PathBuf::from(
        env::var(RESTRICTION_PROBE_DONE).expect("restriction probe completion marker"),
    );
    let ctx = AppContext::new(
        default_language_provider_factory(),
        Config {
            project_root: Some(project),
            gh_read: enabled_gh_read(),
            ..Config::default()
        },
    );
    if case == "untrusted-mcp" {
        ctx.set_harness(Harness::Mcp {
            client: "restriction-probe".to_string(),
        });
    } else {
        assert_eq!(case, "forced-restrict", "unknown restriction probe case");
        ctx.set_harness(Harness::Runner);
    }
    let request = RawRequest {
        id: format!("{case}-github-read"),
        command: "read".to_string(),
        lsp_hints: None,
        session_id: Some(format!("{case}-session")),
        params: json!({
            "file": "issue://7",
            "vision_capability": true,
        }),
    };

    // Subc applies this guard to untrusted MCP requests before command dispatch.
    // Exercising the guard directly keeps the regression fixture hermetic while
    // verifying the same handler boundary used by a forced-restrict bind.
    let response = ctx.with_force_restrict(&request.id, || handle_read(&request, &ctx));
    assert!(
        !response.success,
        "restricted GitHub read unexpectedly succeeded"
    );
    assert_eq!(response.data["code"], "external_fetch_restricted");
    let rendered = serde_json::to_string(&response).expect("serialize restricted response");
    assert!(rendered.contains("Network-backed GitHub reads are unavailable on restricted binds"));
    assert_no_sensitive_material(&rendered);

    for (command, params) in [
        ("outline", json!({ "target": "issue://7" })),
        ("zoom", json!({ "file": "issue://7", "symbols": ["1"] })),
    ] {
        let request = RawRequest {
            id: format!("{case}-github-{command}"),
            command: command.to_string(),
            lsp_hints: None,
            session_id: Some(format!("{case}-session")),
            params,
        };
        let response = ctx.with_force_restrict(&request.id, || match command {
            "outline" => handle_outline(&request, &ctx),
            "zoom" => handle_zoom(&request, &ctx),
            _ => unreachable!(),
        });
        assert!(
            !response.success,
            "restricted {command} unexpectedly succeeded"
        );
        assert_eq!(response.data["code"], "external_fetch_restricted");
    }
    fs::write(completion_marker, case).expect("mark isolated restriction probe complete");
}

fn spawn_proxy_probe() -> (String, mpsc::Receiver<bool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind image-host proxy probe");
    let address = listener
        .local_addr()
        .expect("read image-host proxy address");
    listener
        .set_nonblocking(true)
        .expect("make image-host proxy probe nonblocking");
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(750);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((_stream, _peer)) => {
                    let _ = observed_tx.send(true);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept image-host proxy probe connection: {error}"),
            }
        }
        let _ = observed_tx.send(false);
    });
    (format!("http://{address}"), observed_rx, handle)
}

fn run_isolated_restriction_probe(case: &str) {
    let fixture = tempfile::tempdir().expect("create restriction fixture root");
    let project = fixture.path().join("project");
    let bin_dir = fixture.path().join("bin");
    let gh_log = fixture.path().join("gh-calls.log");
    let completion_marker = fixture.path().join("probe-complete");
    fs::create_dir_all(project.join("issue:")).expect("create filesystem fallthrough fixture");
    fs::write(
        project.join("issue:").join("7"),
        AMBIENT_OPERATOR_CREDENTIAL,
    )
    .expect("write filesystem fallthrough fixture");
    fs::create_dir_all(&bin_dir).expect("create restriction gh directory");
    write_executable(
        &bin_dir.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$AFT_GITHUB_RESTRICTION_GH_LOG"
printf '%s\n' '{"number":7,"title":"unexpected gh execution","state":"OPEN","body":"ghp_operator_ambient_credential\nhttps://github.com/user-attachments/files/7/probe.png","url":"https://github.com/owner/repo/issues/7","comments":[]}'
"#,
    );
    fs::write(&gh_log, "").expect("create restricted gh call log");
    let (proxy_url, image_host_called, proxy_thread) = spawn_proxy_probe();

    let status = Command::new(env::current_exe().expect("find integration test executable"))
        .args(["isolated_restriction_probe", "--nocapture"])
        .env(RESTRICTION_PROBE_CASE, case)
        .env(RESTRICTION_PROBE_PROJECT, &project)
        .env(RESTRICTION_PROBE_DONE, &completion_marker)
        .env(RESTRICTION_GH_LOG, &gh_log)
        .env("PATH", &bin_dir)
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .status()
        .expect("run isolated restriction probe");
    assert!(status.success(), "isolated {case} restriction probe failed");
    assert_eq!(
        fs::read_to_string(&completion_marker).expect("read restriction probe completion marker"),
        case,
        "the isolated test filter must run the intended restriction probe"
    );
    assert!(
        fs::read_to_string(&gh_log)
            .expect("read restricted gh call log")
            .is_empty(),
        "{case} restriction must refuse before any gh invocation"
    );
    assert!(
        !image_host_called
            .recv_timeout(Duration::from_secs(1))
            .expect("image-host proxy probe resolves"),
        "{case} restriction must refuse before any image-host request"
    );
    proxy_thread.join().expect("join image-host proxy probe");
}

#[test]
fn untrusted_mcp_and_forced_restrict_binds_refuse_before_gh_image_or_filesystem_work() {
    for case in ["untrusted-mcp", "forced-restrict"] {
        run_isolated_restriction_probe(case);
    }
}

#[test]
fn github_cli_failures_are_actionable_redacted_and_missing_cli_is_immediate() {
    let fixture = tempfile::tempdir().expect("create gh failure fixture root");
    let bin_dir = fixture.path().join("bin");
    write_failure_gh(&bin_dir);
    let path = path_with(&bin_dir);
    let project = tempfile::tempdir().expect("create failing gh project");
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    configure_gh_read(&mut aft, project.path(), "runner");

    for (number, actionable_message) in [
        (404, "private resource"),
        (405, "resource does not exist"),
        (401, "Bad credentials"),
    ] {
        let response = read_request(
            &mut aft,
            &format!("github-error-{number}"),
            &format!("issue://owner/repo/{number}"),
        );
        assert_eq!(
            response["success"], false,
            "failure became success: {response:#}"
        );
        assert_eq!(response["code"], "github_fetch_failed");
        let rendered = response.to_string();
        assert!(
            rendered.contains(actionable_message),
            "GitHub failure must retain actionable context: {rendered}"
        );
        assert!(
            rendered.contains("[redacted]"),
            "failure must redact credentials: {rendered}"
        );
        assert_no_sensitive_material(&rendered);
    }
    assert!(aft.shutdown().success());

    let missing_bin = tempfile::tempdir().expect("create empty PATH directory");
    let missing_project = tempfile::tempdir().expect("create missing-cli project");
    let mut missing = AftProcess::spawn_with_env(&[("PATH", missing_bin.path().as_os_str())]);
    configure_gh_read(&mut missing, missing_project.path(), "runner");
    let started = Instant::now();
    let response = read_request(&mut missing, "missing-gh", RESOURCE);
    let elapsed = started.elapsed();
    assert_eq!(
        response["success"], false,
        "missing gh became success: {response:#}"
    );
    assert_eq!(response["code"], "github_cli_missing");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Install GitHub CLI and authenticate it with `gh auth login`"),
        "missing gh response must include remediation: {response:#}"
    );
    assert!(
        // Distinguishes an immediate typed error from a deferred retry ladder
        // (tens of seconds); generous because parallel-suite load plus the
        // fixture's first-exec assessment tax blew tighter bounds twice.
        elapsed < Duration::from_secs(8),
        "missing gh must return a typed error instead of hanging or queuing: {elapsed:?}"
    );
    assert!(missing.shutdown().success());
}

struct FixtureClock(AtomicI64);

impl FixtureClock {
    fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }

    fn set(&self, now_ms: i64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}

impl GithubReadClock for FixtureClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct InstrumentedFetcher {
    calls: AtomicUsize,
    refresh_started: mpsc::SyncSender<()>,
}

impl InstrumentedFetcher {
    fn new(refresh_started: mpsc::SyncSender<()>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            refresh_started,
        }
    }
}

impl GithubFetcher for InstrumentedFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 3 {
            self.refresh_started
                .send(())
                .expect("report background refresh start");
        }
        Ok(GithubDocument {
            repository: request
                .resource
                .repository
                .clone()
                .unwrap_or_else(|| "owner/repo".to_string()),
            kind: GithubDocumentKind::Issue,
            number: request.resource.number,
            title: format!("fixture fetch {call}"),
            state: "OPEN".to_string(),
            body: "https://github.com/user-attachments/files/7/fixture.png".to_string(),
            ..GithubDocument::default()
        })
    }
}

struct FixtureImageDownloader(AtomicUsize);

impl GithubImageDownloader for FixtureImageDownloader {
    fn download(
        &self,
        url: &Url,
        _maximum_bytes: usize,
    ) -> Result<Option<DownloadedGithubImage>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(DownloadedGithubImage {
            final_url: url.clone(),
            mime: "image/png".to_string(),
            bytes: vec![137, 80, 78, 71, 13, 10, 26, 10],
        }))
    }
}

fn complete_deferred(start: GithubReadStart) -> GithubReadCompletion {
    let GithubReadStart::Deferred(deferred) = start else {
        panic!("network-bound GitHub read must return a deferred response");
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(result) = deferred.try_complete() {
            return result.expect("fixture GitHub read completes");
        }
        assert!(Instant::now() < deadline, "deferred GitHub read timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn cache_miss_repeat_read_and_image_download_paths_use_deferred_responses() {
    // Fetch-always contract: every read is a live fetch through the deferred
    // machinery; the cache exists only as a fetch-failure fallback, so repeat
    // reads fetch again rather than serving TTL-fresh or stale-refresh copies.
    let storage = tempfile::tempdir().expect("create deferred-path cache storage");
    let (refresh_started_tx, _refresh_started_rx) = mpsc::sync_channel(1);
    let fetcher = Arc::new(InstrumentedFetcher::new(refresh_started_tx));
    let downloader = Arc::new(FixtureImageDownloader(AtomicUsize::new(0)));
    let clock = Arc::new(FixtureClock::new(1_000));
    let engine = GithubReadEngine::new(
        sqlite_cache_store(storage.path().join("aft.db")),
        fetcher.clone(),
        downloader.clone(),
        clock.clone(),
    );

    let first = complete_deferred(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/deferred",
                "principal:deferred",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("start cache-miss read"),
    );
    assert!(first.content.contains("fixture fetch 1"));

    clock.set(2_000);
    let repeat = engine
        .start_resource(
            &enabled_gh_read(),
            RESOURCE,
            "/fixture/deferred",
            "principal:deferred",
            None,
            GithubReadSelector::WholeDocument,
        )
        .expect("start repeat read");
    let repeat = complete_deferred(repeat);
    assert!(
        repeat.content.contains("fixture fetch 2"),
        "a repeat read must live-fetch, never serve the cached copy"
    );

    let vision = engine
        .start_resource(
            &enabled_gh_read(),
            RESOURCE,
            "/fixture/deferred",
            "principal:deferred",
            Some(true),
            GithubReadSelector::WholeDocument,
        )
        .expect("start vision attachment read");
    let vision = complete_deferred(vision);
    assert_eq!(vision.attachments.len(), 1);
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 3);
    assert_eq!(downloader.0.load(Ordering::SeqCst), 1);
}

struct StaticFetcher(GithubDocument);

impl GithubFetcher for StaticFetcher {
    fn fetch(&self, _request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        Ok(self.0.clone())
    }
}

#[test]
fn render_attachment_and_cache_data_do_not_expose_raw_authentication_identity() {
    let storage = tempfile::tempdir().expect("create credential-isolation cache storage");
    let database = storage.path().join("aft.db");
    let engine = GithubReadEngine::new(
        sqlite_cache_store(&database),
        Arc::new(StaticFetcher(GithubDocument {
            repository: "owner/repo".to_string(),
            kind: GithubDocumentKind::Issue,
            number: 7,
            title: "credential isolation fixture".to_string(),
            state: "OPEN".to_string(),
            body: "https://github.com/user-attachments/files/7/fixture.png".to_string(),
            ..GithubDocument::default()
        })),
        Arc::new(FixtureImageDownloader(AtomicUsize::new(0))),
        Arc::new(FixtureClock::new(1_000)),
    );

    let completion = complete_deferred(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/credential-isolation",
                AUTHORISATION_IDENTITY,
                Some(true),
                GithubReadSelector::WholeDocument,
            )
            .expect("start credential-isolation read"),
    );
    let exposed = format!("{completion:?}");
    assert_no_sensitive_material(&exposed);
    assert_eq!(completion.attachments.len(), 1);
    assert_no_sensitive_material(completion.attachments[0].source_url.as_str());

    let persisted = fs::read(&database).expect("read persisted GitHub cache");
    let persisted = String::from_utf8_lossy(&persisted);
    assert_no_raw_credentials(&persisted);
    assert!(
        !persisted.contains(AUTHORISATION_IDENTITY),
        "cache must retain only a nonreversible identity hash, never the raw identity"
    );
}

#[test]

fn slow_github_fetch_does_not_block_sibling_status_or_ordinary_read_on_standalone() {
    let fixture = tempfile::tempdir().expect("create slow-gh fixture root");
    let bin_dir = fixture.path().join("bin");
    let slow_started = fixture.path().join("slow-gh-started");
    let slow_release = fixture.path().join("slow-gh-release");
    // Declare after the fixture TempDir: Rust drops locals in reverse declaration
    // order, so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(slow_release.clone());
    fs::create_dir_all(&bin_dir).expect("create slow-gh bin directory");
    // The fetch blocks on a release file rather than a sleep so the pending
    // window is gated, not timed: siblings answering while the release file is
    // absent is a pure ordering proof, immune to runner load (census S-class).
    write_executable(
        &bin_dir.join("gh"),
        r#"#!/bin/sh
: > "$AFT_SLOW_GH_STARTED"
waited=0
while [ ! -e "$AFT_SLOW_GH_RELEASE" ] && [ "$waited" -lt 300 ]; do
    sleep 0.1
    waited=$((waited + 1))
done
printf '%s\n' '{"number":7,"title":"slow fixture","state":"OPEN","body":"slow body","url":"https://github.com/owner/repo/issues/7","comments":[]}'
"#,
    );
    let ordinary_file = fixture.path().join("ordinary.txt");
    fs::write(&ordinary_file, "ordinary sibling read\n").expect("write ordinary sibling file");
    let path = path_with(&bin_dir);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_SLOW_GH_STARTED", slow_started.as_os_str()),
        ("AFT_SLOW_GH_RELEASE", slow_release.as_os_str()),
    ]);
    configure_gh_read(&mut aft, fixture.path(), "runner");

    aft.send_silent(
        &json!({
            "id": "slow-github-read",
            "command": "read",
            "file": RESOURCE,
        })
        .to_string(),
    );
    // Hang catch only: under parallel-suite load the fixture's first exec can
    // pay the fresh-inode assessment tax well past 1s.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !slow_started.exists() {
        assert!(Instant::now() < deadline, "slow gh fixture did not start");
        thread::sleep(Duration::from_millis(10));
    }

    // The non-blocking DECISION is proven by ordering, not wall-clock: both
    // sibling responses must arrive while the slow fetch (2s fixture sleep)
    // is still pending. The generous send budgets are hang catches only -
    // tight wall-clock bounds here flaked under parallel-suite load (the
    // census S-class shape).
    let siblings_started = Instant::now();
    let status = aft.send_with_timeout(
        &json!({ "id": "sibling-status", "command": "status" }).to_string(),
        Duration::from_secs(8),
    );
    assert_eq!(
        status["success"], true,
        "slow GitHub fetch blocked status: {status:#}"
    );
    assert_eq!(
        status["id"], "sibling-status",
        "sibling status must answer before the slow GitHub read: {status:#}"
    );

    let ordinary = aft.send_with_timeout(
        &json!({
            "id": "sibling-ordinary-read",
            "command": "read",
            "file": ordinary_file,
        })
        .to_string(),
        Duration::from_secs(8),
    );
    assert_eq!(
        ordinary["success"], true,
        "slow GitHub fetch blocked ordinary read: {ordinary:#}"
    );
    assert_eq!(
        ordinary["id"], "sibling-ordinary-read",
        "sibling read must answer before the slow GitHub read: {ordinary:#}"
    );
    let _ = siblings_started;
    // Both siblings answered while the release file was still absent, so the
    // slow fetch was provably pending the whole time - the ordering proof
    // needs no elapsed bound. Release the fixture so shutdown can drain the
    // deferred response.
    assert!(
        !slow_release.exists(),
        "release file must not exist before the test creates it"
    );
    drop(_release_guard);
    assert!(aft.shutdown().success());
}
