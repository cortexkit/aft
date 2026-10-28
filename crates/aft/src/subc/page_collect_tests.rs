//! The page-bundle management operations against a real `AppContext`:
//! which route's policy a collection uses, and that a file the session may
//! not read is never opened.

use super::*;
use crate::subc::test_support::{route_identity_with_harness, test_root};
use std::sync::Mutex;

const HARNESS: &str = "test-harness";
const SESSION: &str = "page-session";

fn core() -> Option<Principal> {
    Some(Principal::Reserved {
        module_id: "prefrontal-core".into(),
    })
}

fn identity(root: &ProjectRootId, trust: BindTrust) -> RouteIdentity {
    // A harness without a plugin prompt: a question the policy would ask
    // is refused outright, and no temp-directory exemption applies.
    route_identity_with_harness(root, SESSION, trust, HARNESS)
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    root: PathBuf,
    outside: PathBuf,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    routes: HashMap<RouteChannel, RouteIdentity>,
}

/// A bound root holding `index.html`, whose references cover every way a
/// file can be out of the session's reach: a configured read_deny folder,
/// symlinks out of the root (to a file, a folder, and an AFT storage
/// database), and SQLite files in the root.
fn fixture(trust: BindTrust) -> Fixture {
    let (root_dir, root_id) = test_root("page-collect-root");
    let outside_dir = tempfile::tempdir().unwrap();
    let root = root_id.as_path().to_path_buf();
    let outside = outside_dir.path().canonicalize().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.sandbox.read_deny = vec![root.join("secret")];
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    let executor = Arc::new(Executor::new());
    executor.register_actor(root_id.clone(), Arc::clone(&ctx));
    let routes = HashMap::from([(route_key(1, 1), identity(&root_id, trust))]);

    std::fs::write(
        root.join("index.html"),
        r#"<!doctype html><img src="ok.png"><img src="secret/key.png"><img src="out.png">
        <img src="dir-link/x.png"><img src="cache.db"><img src="db-link.png"><img src="store.png">"#,
    )
    .unwrap();
    std::fs::write(root.join("ok.png"), "OK").unwrap();
    std::fs::create_dir(root.join("secret")).unwrap();
    std::fs::write(root.join("secret/key.png"), "KEY").unwrap();
    std::fs::write(outside.join("outside.png"), "OUT").unwrap();
    std::fs::write(outside.join("x.png"), "X").unwrap();
    std::fs::write(root.join("cache.db"), "SQLITE").unwrap();
    let storage = ctx.storage_dir();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("aft.db"), "SQLITE").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(outside.join("outside.png"), root.join("out.png")).unwrap();
        symlink(&outside, root.join("dir-link")).unwrap();
        symlink(root.join("cache.db"), root.join("db-link.png")).unwrap();
        symlink(storage.join("aft.db"), root.join("store.png")).unwrap();
    }
    Fixture {
        _dirs: vec![root_dir, outside_dir],
        // Opened paths are fully resolved; compare against the same form.
        root: crate::inspect::job::normalize_path(&root.canonicalize().unwrap()),
        outside,
        ctx,
        executor,
        routes,
    }
}

fn call(f: &Fixture, principal: Option<Principal>, params: Value) -> CollectCall {
    CollectCall {
        params,
        principal,
        routes: f.routes.clone(),
        executor: Arc::clone(&f.executor),
        store: Arc::new(SnapshotStore::open(&f.ctx.storage_dir())),
    }
}

/// Hooks that record every path the reader opens (on a bounded I/O helper
/// thread, so the record is shared) and then open it as production does.
fn recording_hooks() -> (ReadHooks<'static>, Arc<Mutex<Vec<PathBuf>>>) {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&opened);
    let hooks = ReadHooks {
        open: Arc::new(move |root: &Path, target: &Path| {
            record.lock().unwrap().push(target.to_path_buf());
            file_read_exact::open_no_follow(root, target)
        }),
        ..ReadHooks::production()
    };
    (hooks, opened)
}

fn opened_paths(opened: &Arc<Mutex<Vec<PathBuf>>>) -> Vec<PathBuf> {
    opened.lock().unwrap().clone()
}

/// `params` for `page.read` or `page.release`, keyed by the fixture's
/// session as core sends them.
fn keyed(mut params: Value) -> Value {
    params["harness"] = json!(HARNESS);
    params["session"] = json!(SESSION);
    params
}

fn collect_params(entry: &str) -> Value {
    json!({"harness": HARNESS, "session": SESSION, "entry": entry})
}

/// Collect `index.html`, recording every path the reader opens.
fn collect_recording(f: &Fixture) -> (Response, Vec<PathBuf>) {
    let (hooks, opened) = recording_hooks();
    let response = collect_with_hooks(call(f, core(), collect_params("index.html")), hooks);
    (response, opened_paths(&opened))
}

fn omitted(response: &Response) -> Vec<(String, String)> {
    response.data["omitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap().to_string(),
                entry["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn page_collect_denied_references_are_never_opened() {
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let f = fixture(trust);
        let (response, opened) = collect_recording(&f);
        assert!(response.success, "{:?}", response.data);
        let files = response.data["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["index.html", "ok.png"]);
        // Without symlinks (Windows here), the linked names do not exist.
        let linked = if cfg!(unix) { "denied" } else { "missing" };
        let expected = vec![
            ("secret/key.png", "denied"),
            ("out.png", linked),
            ("dir-link/x.png", linked),
            ("cache.db", "denied"),
            ("db-link.png", linked),
            ("store.png", linked),
        ];
        assert_eq!(
            omitted(&response),
            expected
                .into_iter()
                .map(|(path, reason)| (path.to_string(), reason.to_string()))
                .collect::<Vec<_>>()
        );
        // The open-counting hook saw exactly the two readable files: no
        // denied file, symlink target outside the root, or database was
        // ever opened.
        assert_eq!(
            opened,
            vec![f.root.join("index.html"), f.root.join("ok.png")],
            "trust {trust:?}"
        );
        assert!(!opened.iter().any(|path| path.starts_with(&f.outside)));
    }
}

#[test]
fn page_collect_entry_refusals_name_no_denied_contents() {
    let f = fixture(BindTrust::FirstParty);
    std::fs::write(f.root.join("secret/page.html"), "<p>TOP-SECRET</p>").unwrap();
    for (entry, code, reason) in [
        ("secret/page.html", "entry_unreadable", "denied"),
        ("absent.html", "entry_unreadable", "missing"),
        ("ok.png", "entry_not_html", "extension"),
    ] {
        let (hooks, opened) = recording_hooks();
        let response = collect_with_hooks(call(&f, core(), collect_params(entry)), hooks);
        assert_eq!(opened_paths(&opened), Vec::<PathBuf>::new(), "{entry}");
        assert!(!response.success);
        assert_eq!(response.data["code"], code, "{entry}");
        assert_eq!(
            response.data["detail"],
            json!({"reason": reason}),
            "{entry}"
        );
        assert!(!response.data.to_string().contains("TOP-SECRET"));
    }
}

#[test]
fn page_collect_refuses_callers_other_than_core() {
    let f = fixture(BindTrust::FirstParty);
    let store = SnapshotStore::open(&f.ctx.storage_dir());
    for principal in [
        None,
        Some(Principal::Direct),
        Some(Principal::Reserved {
            module_id: "prefrontal".into(),
        }),
        Some(Principal::Reserved {
            module_id: "aft".into(),
        }),
    ] {
        let (hooks, opened) = recording_hooks();
        let response = collect_with_hooks(
            call(&f, principal.clone(), collect_params("index.html")),
            hooks,
        );
        assert_eq!(
            response.data["code"], "page_caller_refused",
            "{principal:?}"
        );
        assert_eq!(
            opened_paths(&opened),
            Vec::<PathBuf>::new(),
            "{principal:?}"
        );
        let ReadReply::Refused(response) = read(
            keyed(json!({"snapshot": "x", "path": "index.html", "offset": 0, "length": 1})),
            principal.as_ref(),
            &store,
        ) else {
            panic!("page.read answered a refused caller");
        };
        assert_eq!(response.data["code"], "page_caller_refused");
        let response = release(keyed(json!({"snapshot": "x"})), principal.as_ref(), &store);
        assert_eq!(response.data["code"], "page_caller_refused");
    }
}

#[test]
fn page_collect_unbound_session_is_refused() {
    let f = fixture(BindTrust::FirstParty);
    let (hooks, opened) = recording_hooks();
    let response = collect_with_hooks(
        call(
            &f,
            core(),
            json!({"harness": HARNESS, "session": "other", "entry": "index.html"}),
        ),
        hooks,
    );
    assert_eq!(response.data["code"], SESSION_NOT_BOUND);
    assert_eq!(opened_paths(&opened), Vec::<PathBuf>::new());
}

fn channel(number: u16) -> RouteChannel {
    route_key(number, 1)
}

fn no_policy(_: &RouteIdentity) -> codemode::SavedPolicyRead {
    Ok(codemode::EffectiveSavedPolicy::default())
}

#[test]
fn page_collect_routes_that_differ_are_ambiguous() {
    let (_dir, root) = test_root("page-collect-routes");
    let first = identity(&root, BindTrust::FirstParty);
    let untrusted = identity(&root, BindTrust::Untrusted);

    assert_eq!(
        choose_route(Vec::new(), Path::new("index.html"), no_policy)
            .unwrap_err()
            .code,
        SESSION_NOT_BOUND
    );
    let chosen = choose_route(
        vec![(channel(1), first.clone()), (channel(2), first.clone())],
        Path::new("index.html"),
        no_policy,
    )
    .unwrap();
    assert!(Arc::ptr_eq(&chosen.0, &first.0));

    let error = choose_route(
        vec![(channel(1), first.clone()), (channel(2), untrusted)],
        Path::new("index.html"),
        no_policy,
    )
    .unwrap_err();
    assert_eq!(error.code, SESSION_ROUTE_AMBIGUOUS);
    let fields = error.detail.as_ref().unwrap()["fields"].clone();
    assert!(
        fields.as_array().unwrap().contains(&json!("trust")),
        "{fields}"
    );

    // Equal access settings but different saved restrictions.
    let mut reads = 0;
    let error = choose_route(
        vec![(channel(1), first.clone()), (channel(2), first.clone())],
        Path::new("index.html"),
        |_| {
            reads += 1;
            Ok(if reads == 1 {
                codemode::EffectiveSavedPolicy::default()
            } else {
                codemode::EffectiveSavedPolicy::confined_for_test()
            })
        },
    )
    .unwrap_err();
    assert_eq!(error.code, SESSION_ROUTE_AMBIGUOUS);
    assert_eq!(error.detail.unwrap()["fields"], json!(["saved_policy"]));
}

#[test]
fn page_collect_prefers_the_route_bound_where_the_page_is() {
    let (_a_dir, root_a) = test_root("page-collect-a");
    let (_b_dir, root_b) = test_root("page-collect-b");
    let a = identity(&root_a, BindTrust::FirstParty);
    let b = identity(&root_b, BindTrust::Untrusted);
    let routes = vec![(channel(1), a.clone()), (channel(2), b.clone())];
    let in_b = root_b.as_path().join("index.html");
    let chosen = choose_route(routes.clone(), &in_b, no_policy).unwrap();
    assert!(Arc::ptr_eq(&chosen.0, &b.0));
    // A relative page names no root, so the routes must agree.
    assert_eq!(
        choose_route(routes, Path::new("index.html"), no_policy)
            .unwrap_err()
            .code,
        SESSION_ROUTE_AMBIGUOUS
    );
}

#[test]
fn page_collect_session_routes_skip_other_sessions_and_codemode_runs() {
    let (_dir, root) = test_root("page-collect-filter");
    let mine = identity(&root, BindTrust::FirstParty);
    let other_session = route_identity_with_harness(&root, "other", BindTrust::FirstParty, HARNESS);
    let other_harness = route_identity_with_harness(&root, SESSION, BindTrust::FirstParty, "pi");
    let mut run = identity(&root, BindTrust::FirstParty);
    Arc::get_mut(&mut run.0).unwrap().call_mode = subc_protocol::call_mode::CallMode::Codemode;
    let routes = HashMap::from([
        (channel(3), mine.clone()),
        (channel(1), mine.clone()),
        (channel(2), other_session),
        (channel(4), other_harness),
        (channel(5), run),
    ]);
    let found = session_routes(&routes, HARNESS, SESSION)
        .into_iter()
        .map(|(channel, _)| channel.channel)
        .collect::<Vec<_>>();
    assert_eq!(found, vec![1, 3]);
}

#[test]
fn page_collect_snapshot_round_trip_through_the_handlers() {
    let f = fixture(BindTrust::FirstParty);
    let store = Arc::new(SnapshotStore::open(&f.ctx.storage_dir()));
    let mut call = call(&f, core(), collect_params("index.html"));
    call.store = Arc::clone(&store);
    let response = collect(call);
    assert!(response.success, "{:?}", response.data);
    let snapshot = response.data["snapshot"].as_str().unwrap().to_string();
    let ReadReply::Bytes(bytes) = read(
        keyed(json!({"snapshot": snapshot, "path": "ok.png", "offset": 1, "length": 10})),
        core().as_ref(),
        &store,
    ) else {
        panic!("page.read refused");
    };
    assert_eq!(bytes, b"K");
    // The snapshot answers even after the project file changes.
    std::fs::write(f.root.join("ok.png"), "CHANGED").unwrap();
    let ReadReply::Bytes(bytes) = read(
        keyed(json!({"snapshot": snapshot, "path": "ok.png", "length": 10})),
        core().as_ref(),
        &store,
    ) else {
        panic!("page.read refused");
    };
    assert_eq!(bytes, b"OK");
    for (params, code) in [
        (
            json!({"snapshot": snapshot, "path": "secret/key.png", "length": 1}),
            "not_in_snapshot",
        ),
        (
            json!({"snapshot": snapshot, "path": "ok.png", "offset": 3, "length": 1}),
            "bad_range",
        ),
        (
            json!({"snapshot": snapshot, "path": "ok.png", "length": 1_048_577}),
            "bad_range",
        ),
        (
            json!({"snapshot": "0000", "path": "ok.png", "length": 1}),
            "snapshot_expired",
        ),
    ] {
        let ReadReply::Refused(response) = read(keyed(params.clone()), core().as_ref(), &store)
        else {
            panic!("{params}: answered");
        };
        assert_eq!(response.data["code"], code, "{params}");
    }
    let released = release(
        keyed(json!({"snapshot": snapshot})),
        core().as_ref(),
        &store,
    );
    assert_eq!(
        released.data,
        json!({"snapshot": snapshot, "released": true})
    );
    let again = release(
        keyed(json!({"snapshot": snapshot})),
        core().as_ref(),
        &store,
    );
    assert_eq!(again.data, json!({"snapshot": snapshot, "released": false}));
}

/// A symlink the page names is re-pointed at a database after its policy
/// check and before it is resolved. The resolved-path storage floor must
/// refuse it before the open: opening and closing a descriptor on a live
/// SQLite file drops this process's locks on it.
#[cfg(unix)]
#[test]
fn page_collect_symlink_swapped_to_a_database_after_the_check_is_never_opened() {
    use std::os::unix::fs::symlink;
    let f = fixture(BindTrust::FirstParty);
    let storage_db = f.ctx.storage_dir().join("aft.db");
    for target in [f.root.join("cache.db"), storage_db] {
        let link = f.root.join("swap.png");
        let _ = std::fs::remove_file(&link);
        symlink(f.root.join("ok.png"), &link).unwrap();
        std::fs::write(f.root.join("swap.html"), r#"<img src="swap.png">"#).unwrap();
        let swapped = Mutex::new(false);
        let swap = |path: &Path| {
            if path.ends_with("swap.png") {
                std::fs::remove_file(path).unwrap();
                symlink(&target, path).unwrap();
                *swapped.lock().unwrap() = true;
            }
        };
        let (hooks, opened) = recording_hooks();
        let response = collect_with_hooks(
            call(&f, core(), collect_params("swap.html")),
            ReadHooks {
                after_check: Some(&swap),
                ..hooks
            },
        );
        assert!(*swapped.lock().unwrap(), "the swap hook never ran");
        assert!(response.success, "{:?}", response.data);
        assert_eq!(
            omitted(&response),
            vec![("swap.png".to_string(), "denied".to_string())],
            "{}",
            target.display()
        );
        assert_eq!(
            opened_paths(&opened),
            vec![f.root.join("swap.html")],
            "a database was opened through the swapped link {}",
            target.display()
        );
    }
}

/// The real `read` tool handler, as production dispatch runs it.
fn read_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::read::handle_read(&request, ctx)
}

/// The routes the parity test compares on: a first-party route whose
/// questions go to a plugin prompt, an untrusted route, a confined scoped
/// worker of each trust, and a route of each trust whose scope saved the
/// full-access approval mode. Full access lets a first-party route read
/// outside its root without asking; an untrusted route's forced root
/// restriction still refuses those paths.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ParityRoute {
    FirstPartyPlugin,
    Untrusted,
    ConfinedFirstParty,
    ConfinedUntrusted,
    FirstPartyFullAccess,
    UntrustedFullAccess,
}

const PARITY_ROUTES: [ParityRoute; 6] = [
    ParityRoute::FirstPartyPlugin,
    ParityRoute::Untrusted,
    ParityRoute::ConfinedFirstParty,
    ParityRoute::ConfinedUntrusted,
    ParityRoute::FirstPartyFullAccess,
    ParityRoute::UntrustedFullAccess,
];

struct ParityFixture {
    _dirs: Vec<tempfile::TempDir>,
    _temp: crate::access_policy::temp::RootsOverride,
    root_id: ProjectRootId,
    /// The file outside the root, as the page spells it.
    outside_path: String,
    outside: PathBuf,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    identity: RouteIdentity,
    paths: Vec<String>,
}

/// Register `identity`'s grant scope in the context's database, as a route
/// bind does, and save the full-access approval mode for it when asked.
fn register_scope(ctx: &AppContext, identity: &RouteIdentity, full_access: bool) {
    access_grants::register(identity, ctx).unwrap();
    if full_access {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        crate::db::access_grants::set_mode(
            &conn,
            &access_grants::policy_scope_key(identity),
            crate::access_policy::ApprovalMode::FullAccess,
        )
        .unwrap();
    }
}

/// One project root and the paths its page names: an allowed file, a
/// read_deny folder, symlinks inside and out of the root, a file outside
/// the root (asked about, or allowed under full access), and a file in a
/// directory the policy treats as system temp (exempt for a plugin host's
/// own reads).
fn parity_fixture(route: ParityRoute) -> ParityFixture {
    let (root_dir, root_id) = test_root("page-parity-root");
    let outside_dir = tempfile::tempdir().unwrap();
    let scratch_dir = tempfile::tempdir().unwrap();
    let root = root_id.as_path().to_path_buf();
    let outside = outside_dir.path().canonicalize().unwrap();
    let scratch = scratch_dir.path().canonicalize().unwrap();
    // Only the scratch directory counts as temp, so the outside file is a
    // real outside-the-root path, not a temp exemption.
    let temp = crate::access_policy::temp::override_roots_for_test(vec![scratch.clone()]);
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.sandbox.read_deny = vec![root.join("secret")];
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    // Saved grant scopes and approval modes live in the context's database.
    let storage = ctx.storage_dir();
    std::fs::create_dir_all(&storage).unwrap();
    ctx.set_db(Arc::new(std::sync::Mutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    std::fs::write(root.join("ok.txt"), "ok").unwrap();
    std::fs::create_dir(root.join("secret")).unwrap();
    std::fs::write(root.join("secret/key.txt"), "key").unwrap();
    std::fs::write(outside.join("outside.txt"), "outside").unwrap();
    std::fs::write(scratch.join("scratch.txt"), "scratch").unwrap();
    let relative = |dir: &Path, file: &str| {
        format!("../{}/{file}", dir.file_name().unwrap().to_str().unwrap())
    };
    // Symlinks are added on Unix only.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut paths = vec![
        "ok.txt".to_string(),
        "secret/key.txt".to_string(),
        relative(&outside, "outside.txt"),
        relative(&scratch, "scratch.txt"),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("ok.txt"), root.join("in-link.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("outside.txt"), root.join("out-link.txt")).unwrap();
        paths.extend(["in-link.txt".to_string(), "out-link.txt".to_string()]);
    }
    let page = paths
        .iter()
        .map(|path| format!(r#"<img src="{path}">"#))
        .collect::<String>();
    std::fs::write(root.join("index.html"), format!("<!doctype html>{page}")).unwrap();

    let (trust, harness, confined) = match route {
        ParityRoute::FirstPartyPlugin => (BindTrust::FirstParty, "opencode", false),
        ParityRoute::Untrusted => (BindTrust::Untrusted, HARNESS, false),
        ParityRoute::ConfinedFirstParty => (BindTrust::FirstParty, HARNESS, true),
        ParityRoute::ConfinedUntrusted => (BindTrust::Untrusted, HARNESS, true),
        ParityRoute::FirstPartyFullAccess => (BindTrust::FirstParty, HARNESS, false),
        ParityRoute::UntrustedFullAccess => (BindTrust::Untrusted, HARNESS, false),
    };
    let mut identity = route_identity_with_harness(&root_id, SESSION, trust, harness);
    if confined {
        // A scoped worker whose saved policy is the root confinement fence.
        Arc::get_mut(&mut identity.0).unwrap().scope = Some(
            serde_json::from_value(json!({
                "owner": {"kind": "direct"}, "ref": "parity-worker", "scope_epoch": 1,
                "kind": "worker", "owner_authorized": true,
            }))
            .unwrap(),
        );
        ctx.remember_confined_policy_key(remote_policy::key(&identity, Some("worker")).unwrap());
    }
    register_scope(
        &ctx,
        &identity,
        matches!(
            route,
            ParityRoute::FirstPartyFullAccess | ParityRoute::UntrustedFullAccess
        ),
    );
    let executor = Arc::new(Executor::new());
    executor.register_actor(root_id.clone(), Arc::clone(&ctx));
    ParityFixture {
        _dirs: vec![root_dir, outside_dir, scratch_dir],
        _temp: temp,
        root_id,
        outside_path: relative(&outside, "outside.txt"),
        outside,
        ctx,
        executor,
        identity,
        paths,
    }
}

/// Whether the `read` tool, run through the real tool-call job with the
/// route's own ask route, reads `path`.
async fn read_tool_allows(f: &ParityFixture, path: &str, request: usize) -> bool {
    read_tool_allows_on(f, &f.identity, path, request).await
}

/// [`read_tool_allows`] on another route of the same root.
async fn read_tool_allows_on(
    f: &ParityFixture,
    identity: &RouteIdentity,
    path: &str,
    request: usize,
) -> bool {
    let request_id = format!("parity-read-{request}");
    let arguments = json!({ "path": path });
    let run = PlainToolRun {
        speech_seed: None,
        bare_name: "read".into(),
        arguments: arguments.clone(),
        format_context: Default::default(),
        tool_call_context: ToolCallContext {
            project_root: identity.project_root.clone(),
            session_id: Some(identity.session.clone()),
            request_id: request_id.clone(),
            diagnostics_on_edit: false,
            preview: false,
            edit_slot_survives: None,
            report_registration_downgrade: false,
            standard_edit_grammar: true,
            disabled_tools: None,
            worker_session: false,
            host_tool_names: None,
        },
        dispatch: read_dispatch,
        bind_trust: identity.trust,
        identity: identity.clone(),
        request_id,
        ledger_key: None,
        io_budget: crate::bounded_io::Budget::after(Duration::from_secs(30)),
        approved_io_window: None,
        reply_deadline: Instant::now() + Duration::from_secs(30),
    };
    let (done, completion) = oneshot::channel();
    let job = run.job(
        access_grants::ask_route_for_call(identity, "read", &arguments),
        PhaseTrace::new(Instant::now()),
        done,
    );
    let response = job(&f.ctx);
    completion.await.unwrap();
    response.success
}

/// Whether `page.collect` of the fixture's page puts `path` in the bundle.
fn page_collect_verdicts(f: &ParityFixture) -> HashMap<String, bool> {
    let call = CollectCall {
        params: json!({
            "harness": f.identity.harness, "session": SESSION, "entry": "index.html",
        }),
        principal: core(),
        routes: HashMap::from([(route_key(1, 1), f.identity.clone())]),
        executor: Arc::clone(&f.executor),
        store: Arc::new(SnapshotStore::open(&f.ctx.storage_dir())),
    };
    let response = collect(call);
    assert!(response.success, "{:?}", response.data);
    let mut verdicts = HashMap::new();
    for file in response.data["files"].as_array().unwrap() {
        verdicts.insert(file["path"].as_str().unwrap().to_string(), true);
    }
    for omitted in response.data["omitted"].as_array().unwrap() {
        assert_eq!(omitted["reason"], "denied", "{omitted}");
        verdicts.insert(omitted["path"].as_str().unwrap().to_string(), false);
    }
    verdicts
}

/// For every path of one fixture, on each route, `page.collect` includes
/// exactly the files the session's `read` tool reads: both take their
/// per-call policy from `call_policy::with_tool_call_policy`.
#[tokio::test]
async fn page_collect_reads_exactly_what_the_read_tool_reads() {
    let mut request = 0;
    for route in PARITY_ROUTES {
        let f = parity_fixture(route);
        let mut read = Vec::new();
        for path in &f.paths {
            request += 1;
            read.push((path.clone(), read_tool_allows(&f, path, request).await));
        }
        let collected = page_collect_verdicts(&f);
        let collected = f
            .paths
            .iter()
            .map(|path| (path.clone(), collected[path.as_str()]))
            .collect::<Vec<_>>();
        println!("{route:?}: {read:?}");
        assert_eq!(read, collected, "{route:?}: read tool vs page.collect");
        // The fixture separates allowed from denied on every route.
        assert!(
            read.iter().any(|(_, allowed)| *allowed),
            "{route:?}: {read:?}"
        );
        assert!(
            read.iter().any(|(_, allowed)| !*allowed),
            "{route:?}: {read:?}"
        );
        // Full access reaches outside the root only without forced root
        // restriction, so the comparison covers that restriction too.
        if matches!(
            route,
            ParityRoute::FirstPartyFullAccess | ParityRoute::UntrustedFullAccess
        ) {
            let outside = read.iter().find(|(path, _)| *path == f.outside_path);
            assert_eq!(
                outside.map(|(_, allowed)| *allowed),
                Some(route == ParityRoute::FirstPartyFullAccess),
                "{route:?}: {read:?}"
            );
        }
    }
}

/// An untrusted bind's root restriction is forced: a page reference that
/// leaves the project root is denied and never opened, even when the
/// session's saved approval mode is full access and a first-party session
/// of the same root reads that file.
#[tokio::test]
async fn page_collect_untrusted_session_never_reaches_outside_its_root() {
    let f = parity_fixture(ParityRoute::UntrustedFullAccess);
    assert_eq!(f.identity.trust, BindTrust::Untrusted);

    // A first-party session of the same root, also with full access, reads
    // the outside file.
    let first_party =
        route_identity_with_harness(&f.root_id, "first-party", BindTrust::FirstParty, HARNESS);
    register_scope(&f.ctx, &first_party, true);
    assert!(read_tool_allows_on(&f, &first_party, &f.outside_path, 1).await);

    // The untrusted session's read tool refuses it.
    assert!(!read_tool_allows(&f, &f.outside_path, 2).await);

    // And page.collect leaves it out, without opening it.
    let (hooks, opened) = recording_hooks();
    let response = collect_with_hooks(
        CollectCall {
            params: json!({"harness": HARNESS, "session": SESSION, "entry": "index.html"}),
            principal: core(),
            routes: HashMap::from([(route_key(1, 1), f.identity.clone())]),
            executor: Arc::clone(&f.executor),
            store: Arc::new(SnapshotStore::open(&f.ctx.storage_dir())),
        },
        hooks,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(
        omitted(&response).contains(&(f.outside_path.clone(), "denied".to_string())),
        "{:?}",
        response.data
    );
    let opened = opened_paths(&opened);
    assert!(
        !opened.iter().any(|path| path.starts_with(&f.outside)),
        "a file outside the untrusted session's root was opened: {opened:?}"
    );
}

/// A `<base href>` that leads out of the session's root moves the page's
/// references there, where the session's read policy refuses them: they are
/// listed as denied and never opened.
#[test]
fn page_collect_base_leaving_the_root_is_denied_and_never_opened() {
    let f = fixture(BindTrust::FirstParty);
    let outside_name = f.outside.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        f.root.join("based.html"),
        format!(r#"<base href="../{outside_name}/"><img src="outside.png"><img src="x.png">"#),
    )
    .unwrap();
    let (hooks, opened) = recording_hooks();
    let response = collect_with_hooks(call(&f, core(), collect_params("based.html")), hooks);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(
        omitted(&response),
        vec![
            (
                format!("../{outside_name}/outside.png"),
                "denied".to_string()
            ),
            (format!("../{outside_name}/x.png"), "denied".to_string()),
        ]
    );
    assert_eq!(opened_paths(&opened), vec![f.root.join("based.html")]);
}

/// Longest a test stall holds a read. Past it the read goes on, so a reader
/// that waits for its reads (instead of bounding them) is late, not hung.
const STALL_MAX: Duration = Duration::from_secs(4);

/// Holds every read of a file named `name` on the reader's I/O helper, as a
/// hung network filesystem would, until dropped or [`STALL_MAX`] passes.
struct Stall(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Drop for Stall {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
}

fn stall_reads_of(name: &'static str) -> (BeforeRead, Stall) {
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let gate = Arc::clone(&released);
    let hook: BeforeRead = Arc::new(move |path: &Path| {
        if path.ends_with(name) {
            let (released, wake) = &*gate;
            let guard = released.lock().unwrap();
            let _ = wake
                .wait_timeout_while(guard, STALL_MAX, |released| !*released)
                .unwrap();
        }
    });
    (hook, Stall(released))
}

fn stalled_call(f: &Fixture, store: &Arc<SnapshotStore>, entry: &str) -> CollectCall {
    CollectCall {
        params: json!({
            "harness": HARNESS, "session": SESSION, "entry": entry,
            "limits": {"deadline_ms": 300},
        }),
        principal: core(),
        routes: f.routes.clone(),
        executor: Arc::clone(&f.executor),
        store: Arc::clone(store),
    }
}

/// A referenced file whose read stalls is listed as past the deadline, and
/// the collection returns at its deadline, not when the read finishes.
#[test]
fn page_collect_stalled_read_is_a_deadline_omission() {
    let f = fixture(BindTrust::FirstParty);
    std::fs::write(f.root.join("stall.html"), r#"<img src="ok.png">"#).unwrap();
    let store = Arc::new(SnapshotStore::open(&f.ctx.storage_dir()));
    let (before_read, _stall) = stall_reads_of("ok.png");
    let hooks = ReadHooks {
        before_read: Some(before_read),
        ..ReadHooks::production()
    };
    let started = Instant::now();
    let response = collect_with_hooks(stalled_call(&f, &store, "stall.html"), hooks);
    let elapsed = started.elapsed();
    assert!(response.success, "{:?}", response.data);
    assert_eq!(
        omitted(&response),
        vec![("ok.png".to_string(), "deadline".to_string())]
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "returned after {elapsed:?}"
    );
}

/// Four collections whose pages stall each give their snapshot place back
/// when they return at the deadline, so a fifth collection still runs.
#[test]
fn page_collect_stalled_collections_free_their_snapshot_places() {
    let f = fixture(BindTrust::FirstParty);
    std::fs::write(f.root.join("stall.html"), r#"<img src="ok.png">"#).unwrap();
    let store = Arc::new(SnapshotStore::open(&f.ctx.storage_dir()));
    let (before_read, _stall) = stall_reads_of("stall.html");
    for attempt in 0..crate::page_collect::snapshot::MAX_LIVE {
        let hooks = ReadHooks {
            before_read: Some(Arc::clone(&before_read)),
            ..ReadHooks::production()
        };
        let started = Instant::now();
        let response = collect_with_hooks(stalled_call(&f, &store, "stall.html"), hooks);
        let elapsed = started.elapsed();
        assert_eq!(
            response.data["code"], "entry_unreadable",
            "{attempt}: {:?}",
            response.data
        );
        assert_eq!(response.data["detail"], json!({"reason": "deadline"}));
        // The page gets at least a second (the page-read minimum).
        assert!(
            elapsed < Duration::from_secs(3),
            "{attempt}: returned after {elapsed:?}"
        );
        assert_eq!(
            store.live_count(),
            0,
            "{attempt}: a stalled collection kept its place"
        );
    }
    let response = collect_with_hooks(
        stalled_call(&f, &store, "index.html"),
        ReadHooks::production(),
    );
    assert!(response.success, "{:?}", response.data);
}

/// `page.read` and `page.release` name the session a snapshot is for. Under
/// any other harness or session a snapshot is answered exactly like one
/// that does not exist, and is not released.
#[test]
fn page_collect_read_and_release_are_keyed_by_session() {
    let f = fixture(BindTrust::FirstParty);
    let store = Arc::new(SnapshotStore::open(&f.ctx.storage_dir()));
    let mut call = call(&f, core(), collect_params("index.html"));
    call.store = Arc::clone(&store);
    let response = collect(call);
    assert!(response.success, "{:?}", response.data);
    let snapshot = response.data["snapshot"].as_str().unwrap().to_string();
    let read_as = |harness: &str, session: &str| {
        read(
            json!({"harness": harness, "session": session, "snapshot": snapshot,
                   "path": "ok.png", "offset": 0, "length": 10}),
            core().as_ref(),
            &store,
        )
    };
    let ReadReply::Bytes(bytes) = read_as(HARNESS, SESSION) else {
        panic!("the snapshot's own session was refused");
    };
    assert_eq!(bytes, b"OK");

    // The unknown-snapshot refusal, for comparison.
    let ReadReply::Refused(unknown) = read(
        keyed(json!({"snapshot": "0".repeat(32), "path": "ok.png", "length": 10})),
        core().as_ref(),
        &store,
    ) else {
        panic!("an unknown snapshot was read");
    };
    for (harness, session) in [("other-harness", SESSION), (HARNESS, "other-session")] {
        let ReadReply::Refused(refused) = read_as(harness, session) else {
            panic!("{harness}/{session} read another session's snapshot");
        };
        assert_eq!(refused.data, unknown.data, "{harness}/{session}");
        let released = release(
            json!({"harness": harness, "session": session, "snapshot": snapshot}),
            core().as_ref(),
            &store,
        );
        assert_eq!(
            released.data,
            json!({"snapshot": snapshot, "released": false}),
            "{harness}/{session}"
        );
    }
    // Still there for its own session.
    assert!(matches!(read_as(HARNESS, SESSION), ReadReply::Bytes(_)));

    // Both names are required.
    for params in [
        json!({"session": SESSION, "snapshot": snapshot, "path": "ok.png", "length": 1}),
        json!({"harness": HARNESS, "snapshot": snapshot, "path": "ok.png", "length": 1}),
    ] {
        let ReadReply::Refused(response) = read(params.clone(), core().as_ref(), &store) else {
            panic!("{params}: answered");
        };
        assert_eq!(response.data["code"], "invalid_request", "{params}");
    }
    for params in [
        json!({"session": SESSION, "snapshot": snapshot}),
        json!({"harness": HARNESS, "snapshot": snapshot}),
    ] {
        let response = release(params.clone(), core().as_ref(), &store);
        assert_eq!(response.data["code"], "invalid_request", "{params}");
    }
    assert!(matches!(read_as(HARNESS, SESSION), ReadReply::Bytes(_)));
}
