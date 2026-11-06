//! Parent folders on the data layer: a folder of repositories answers from
//! a checkout of each child repository, checked against what the fixture's
//! files hold.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::data_layer::checkout::DataError;
use crate::data_layer::runtime::DataCheckout;
use crate::protocol::{RawRequest, Response};
use crate::views::parent::Plane;

const DEADLINE: Duration = Duration::from_secs(180);
/// A line each committed crate holds twice.
const COMMON: &str = "PARENT_DATA_LAYER_COMMON_TOKEN";
/// A line each bare child holds once; no crate holds it.
const BARE: &str = "PARENT_DATA_LAYER_BARE_TOKEN";

fn write(root: &Path, path: &str, text: &str) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_env::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A committed Rust crate with a caller of `target` and two lines holding
/// [`COMMON`].
fn init_repo(root: &Path, name: &str) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "parent@example.test"]);
    git(root, &["config", "user.name", "Parent Test"]);
    write(
        root,
        "Cargo.toml",
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    );
    write(
        root,
        "src/lib.rs",
        &format!(
            "mod util;\n\npub fn target() -> u32 {{\n    util::helper()\n}}\n\npub fn caller() -> u32 {{\n    target()\n}}\n\n// {COMMON} in {name}\n"
        ),
    );
    write(
        root,
        "src/util.rs",
        &format!("pub fn helper() -> u32 {{\n    1\n}}\n// {COMMON}\n"),
    );
    write(root, "notes.txt", &format!("notes for {name}\n"));
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "initial"]);
}

/// A folder discovery takes for a repository, never bound by a session of
/// its own: an empty `.git` and one text file.
fn bare_repo(root: &Path, index: usize) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "notes.txt", &format!("{BARE} {index}\n"));
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).unwrap()
}

/// A session bound to `root` through configure, with views and the call
/// graph on or off.
fn configure(root: &Path, storage: &Path, views: bool, callgraph: bool) -> Arc<AppContext> {
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        },
    ));
    let doc = json!({
        "indexes": {"trigram": true, "semantic": false, "callgraph": callgraph},
        "views": {"enabled": views},
    });
    let response = crate::commands::configure::handle_configure(
        &request(json!({
            "id": "configure",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": [{"tier": "user", "source": "/u/aft.jsonc", "doc": doc.to_string()}],
        })),
        &ctx,
    );
    assert!(response.success, "configure failed: {response:?}");
    crate::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    ctx
}

fn callgraph_request(command: &str, file: &str) -> RawRequest {
    request(json!({
        "id": "parent-callgraph",
        "command": command,
        "file": file,
        "symbol": "target",
    }))
}

fn grep(ctx: &AppContext, pattern: &str) -> Value {
    crate::commands::grep::handle_grep(
        &request(json!({
            "id": "parent-grep",
            "command": "grep",
            "pattern": pattern,
            "max_results": 5_000,
        })),
        ctx,
    )
    .data
}

fn glob(ctx: &AppContext, pattern: &str) -> Value {
    crate::commands::glob::handle_glob(
        &request(json!({"id": "parent-glob", "command": "glob", "pattern": pattern})),
        ctx,
    )
    .data
}

/// A call-graph command as the parent session answers it.
fn routed(ctx: &AppContext, req: &RawRequest) -> Response {
    crate::views::parent::route(req, ctx).expect("a parent folder answers this command itself")
}

fn gaps(reply: &Value) -> Vec<Value> {
    reply["gaps"].as_array().cloned().unwrap_or_default()
}

fn child_gap(path: &str, reason: Option<String>) -> Value {
    json!({"kind": "parent_child_unavailable", "path": path, "reason": reason.unwrap()})
}

/// An executor that refuses every job: its root was registered, then
/// unbound. The returned executor and context keep it that way.
fn refusing_executor() -> (
    crate::executor::RootMaintenanceExecutor,
    crate::executor::Executor,
    Arc<AppContext>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    ));
    ctx.mark_subc_bound();
    let executor = crate::executor::Executor::new();
    let root_id = crate::path_identity::ProjectRootId::from_path(&root).unwrap();
    assert!(executor.register_actor(root_id, Arc::clone(&ctx)));
    let maintenance = ctx
        .callgraph_executor_for_test()
        .expect("registering the actor installs its executor");
    ctx.mark_subc_unbound();
    (maintenance, executor, ctx, dir)
}

/// Asks `ask` until `done` holds for its answer, for up to [`DEADLINE`].
fn settled<T: std::fmt::Debug>(ask: impl Fn() -> T, done: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let answer = ask();
        if done(&answer) {
            return answer;
        }
        assert!(Instant::now() < deadline, "never settled: {answer:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The parent session `ctx` is bound to, once its worker's first round
/// has loaded the children.
fn parent_session(ctx: &AppContext) -> Arc<crate::views::parent::ParentSession> {
    let deadline = Instant::now() + DEADLINE;
    let session = loop {
        crate::runtime_drain::drain_deferred_configure_maintenance(ctx);
        if let Some(session) = crate::views::parent::session_for(ctx) {
            break session;
        }
        assert!(Instant::now() < deadline, "no parent session started");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        session.wait_rounds(1, DEADLINE),
        "the first load never ended"
    );
    session
}

fn child_gaps(reply: &Value) -> Vec<Value> {
    gaps(reply)
        .into_iter()
        .filter(|gap| gap["kind"] == "parent_child_unavailable")
        .collect()
}

fn matched_files(reply: &Value) -> Vec<String> {
    reply["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("grep failed: {reply:#}"))
        .iter()
        .map(|row| row["file"].as_str().unwrap().replace('\\', "/"))
        .collect()
}

/// The files the reply names, relative to `root`, sorted.
fn relative_files(root: &Path, files: Vec<String>) -> Vec<String> {
    let prefix = format!("{}/", root.display()).replace('\\', "/");
    let mut files = files
        .into_iter()
        .map(|file| {
            file.strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("{file} is not under {prefix}"))
                .to_string()
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// A folder of 66 repositories: `alpha`, `beta` and `gamma` committed
/// crates, `building` and `failed` crates whose checkouts never finish a
/// build, and 61 bare folders, the last two past the 64-repository cap. With
/// the data layer on, the parent answers grep, glob and the call graph from
/// each served child's own checkout, built on first use, with named gaps for
/// a child still building, a child whose build failed, the children past the
/// cap and the files outside every child.
#[test]
fn a_parent_folder_answers_from_its_childrens_checkouts() {
    crate::views::parent::set_default_refresh_interval(Duration::from_millis(100));
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["alpha", "beta", "building", "failed", "gamma"] {
        init_repo(&root.join(name), name);
    }
    for index in 0..61 {
        bare_repo(&root.join(format!("zz-{index:02}")), index);
    }
    write(&root, "README.md", "a parent folder of repositories\n");

    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        configure(&root, &storage, true, true)
    };
    let session = parent_session(&parent);
    assert_eq!(session.children().len(), 64);

    // `building` has a checkout whose build never runs, and `failed` one
    // whose last build failed and none runs since.
    let (refusing, _executor, _unbound, _dir) = refusing_executor();
    for name in ["building", "failed"] {
        let checkout =
            DataCheckout::open(&root.join(name), &storage, Some(refusing.clone())).unwrap();
        if name == "failed" {
            checkout.note_outcome_for_test(
                "build",
                &Err::<(), _>(DataError::Scan("the fixture's build failed".into())),
            );
        }
        parent
            .data_children()
            .insert_for_test(session.root(), &root.join(name), checkout);
    }
    let failure = "AFT could not scan this checkout: the fixture's build failed".to_string();

    let named = vec![
        child_gap("building", Plane::<()>::Loading.gap_reason("trigram")),
        child_gap(
            "failed",
            Plane::<()>::Gap(failure.clone()).gap_reason("trigram"),
        ),
    ];
    // Every other child builds its checkout on first use.
    let data_grep = settled(|| grep(&parent, COMMON), |reply| child_gaps(reply) == named);
    // `init_repo` writes COMMON once in each crate's src/lib.rs and once in
    // its src/util.rs; `building` and `failed` cannot answer.
    assert_eq!(
        relative_files(&root, matched_files(&data_grep)),
        [
            "alpha/src/lib.rs",
            "alpha/src/util.rs",
            "beta/src/lib.rs",
            "beta/src/util.rs",
            "gamma/src/lib.rs",
            "gamma/src/util.rs",
        ],
        "{data_grep:#}"
    );
    // The two bare folders past the cap, and README.md, which no child holds.
    assert_eq!(
        gaps(&data_grep),
        [
            named.clone(),
            vec![
                json!({
                    "kind": "parent_children_over_cap",
                    "path": ".",
                    "reason": "2 child repositories beyond the 64-repository cap were not searched: zz-59, zz-60",
                }),
                json!({
                    "kind": "outside_child_repositories",
                    "path": "README.md",
                    "reason": "holds files outside every child repository; a parent folder searches only its repositories",
                }),
            ],
        ]
        .concat(),
        "{data_grep:#}"
    );
    // Each bare child within the cap holds BARE once, in notes.txt.
    let data_bare = grep(&parent, BARE);
    assert_eq!(
        relative_files(&root, matched_files(&data_bare)),
        (0..59)
            .map(|index| format!("zz-{index:02}/notes.txt"))
            .collect::<Vec<_>>()
    );
    // The crates' two Rust files each; the bare folders hold none.
    let data_glob = glob(&parent, "**/*.rs");
    let globbed = data_glob["files"]
        .as_array()
        .unwrap_or_else(|| panic!("glob failed: {data_glob:#}"))
        .iter()
        .map(|file| file.as_str().unwrap().replace('\\', "/"))
        .collect();
    assert_eq!(
        relative_files(&root, globbed),
        [
            "alpha/src/lib.rs",
            "alpha/src/util.rs",
            "beta/src/lib.rs",
            "beta/src/util.rs",
            "gamma/src/lib.rs",
            "gamma/src/util.rs",
        ],
        "{data_glob:#}"
    );
    assert_eq!(child_gaps(&data_glob), named);

    // The call graph, current, so without `as_of`. In alpha's src/lib.rs,
    // `caller` calls `target`, and `target` calls `util::helper`.
    let alpha = "alpha/src/lib.rs";
    let callers = settled(
        || routed(&parent, &callgraph_request("callers", alpha)),
        |answer| answer.success,
    );
    let text = callers.data.to_string();
    assert!(text.contains("alpha/src/lib.rs"), "{:#}", callers.data);
    assert!(text.contains("caller"), "{:#}", callers.data);
    assert!(callers.data.get("as_of").is_none(), "{:#}", callers.data);
    let tree = settled(
        || routed(&parent, &callgraph_request("call_tree", alpha)),
        |answer| answer.success,
    );
    assert!(tree.data.to_string().contains("helper"), "{:#}", tree.data);
    assert!(tree.data.get("as_of").is_none(), "{:#}", tree.data);
    let building = routed(
        &parent,
        &callgraph_request("callers", "building/src/lib.rs"),
    );
    assert_eq!(building.data["code"], "parent_child_unavailable");
    assert_eq!(
        gaps(&building.data),
        [child_gap(
            "building",
            Plane::<()>::Loading.gap_reason("call graph")
        )]
    );
    let failed_gap = child_gap("failed", Plane::<()>::Gap(failure).gap_reason("call graph"));
    let failed = settled(
        || routed(&parent, &callgraph_request("callers", "failed/src/lib.rs")),
        |answer| gaps(&answer.data) == [failed_gap.clone()],
    );
    assert_eq!(failed.data["code"], "parent_child_unavailable");
    // A file in a child past the cap belongs to no served child.
    let capped = routed(&parent, &callgraph_request("callers", "zz-60/notes.txt"));
    assert!(!capped.success);
    assert_eq!(capped.data["code"], "parent_child_unavailable");
    assert_eq!(
        gaps(&capped.data),
        [json!({
            "kind": "outside_child_repositories",
            "path": "zz-60/notes.txt",
            "reason": "outside every child repository; a parent folder has no call graph of its own",
        })]
    );

    // The parent folder's own root binds nothing; each served child has
    // its own checkout.
    assert!(parent.data_checkout().is_none());
    assert_eq!(parent.data_children().bound_for_test().len(), 64);
}

/// The children's checkouts are built as the parent root's maintenance:
/// while the parent root is unbound none is built and each child is named
/// as still loading; once the root is bound again, the next query starts
/// their builds. An edit the parent's watcher reports reaches the child
/// that holds it.
#[test]
fn children_build_nothing_while_the_parent_root_is_unbound() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["one", "two"] {
        init_repo(&root.join(name), name);
    }
    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        configure(&root, &storage, true, true)
    };
    let _session = parent_session(&parent);
    parent.mark_subc_bound();
    let executor = crate::executor::Executor::new();
    let root_id = crate::path_identity::ProjectRootId::from_path(&root).unwrap();
    assert!(executor.register_actor(root_id, Arc::clone(&parent)));
    parent.mark_subc_unbound();

    let loading = vec![
        child_gap("one", Plane::<()>::Loading.gap_reason("trigram")),
        child_gap("two", Plane::<()>::Loading.gap_reason("trigram")),
    ];
    assert_eq!(child_gaps(&grep(&parent, COMMON)), loading);
    std::thread::sleep(Duration::from_millis(500));
    let bound = parent.data_children().bound_for_test();
    assert_eq!(bound.len(), 2);
    assert!(
        bound.iter().all(|checkout| checkout.view().is_none()),
        "a child of an unbound parent root was built"
    );
    assert_eq!(child_gaps(&grep(&parent, COMMON)), loading);

    parent.mark_subc_bound();
    let served = settled(
        || grep(&parent, COMMON),
        |reply| reply.get("gaps").is_none() && matched_files(reply).len() == 4,
    );
    assert_eq!(served["complete"], true, "{served:#}");

    let added = write(
        &root.join("one"),
        "src/added.rs",
        "// PARENT_DATA_LAYER_ADDED_TOKEN\n",
    );
    parent.data_note_paths(&[added]);
    let found = matched_files(&grep(&parent, "PARENT_DATA_LAYER_ADDED_TOKEN"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ends_with("one/src/added.rs"), "{found:?}");
}

/// With the data layer on, a parent folder's `aft_search` reads each
/// child's meaning plane without filling it: `alpha`, whose vectors another
/// checkout of its repository stored, answers from them; `beta`, whose store
/// holds none, is the named gap; and no file of either child is embedded,
/// since semantic search is not admitted for a repository no session opened.
#[test]
fn a_parent_folders_search_reads_its_childrens_stored_vectors_and_fills_none() {
    use crate::config::{SemanticBackend, SemanticBackendConfig};
    use crate::data_layer::meaning::MeaningState;
    crate::views::parent::set_default_refresh_interval(Duration::from_millis(100));
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["alpha", "beta"] {
        init_repo(&root.join(name), name);
    }
    let (base_url, gate) = crate::data_layer::tools::search::meaning::tests::serve("never-held");
    gate.open();
    let semantic = SemanticBackendConfig {
        backend: SemanticBackend::OpenAiCompatible,
        model: "test-hashed".to_string(),
        base_url: Some(base_url.clone()),
        timeout_ms: 120_000,
        query_timeout_ms: 30_000,
        ..SemanticBackendConfig::default()
    };
    // A session of its own filled alpha's vectors; nothing filled beta's.
    let alpha_fill = {
        let _git = crate::test_env::hermetic_git_env_guard();
        crate::data_layer::surfaces::fill_root(&root.join("alpha"), &storage, &semantic)
            .expect("alpha's fill")
    };
    assert!(alpha_fill.chunks > 0, "{alpha_fill:?}");

    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..crate::config::Config::default()
            },
        ));
        let doc = json!({
            "indexes": {"trigram": true, "semantic": true, "callgraph": false},
            "views": {"enabled": true},
            "semantic": {
                "backend": "openai_compatible",
                "model": "test-hashed",
                "base_url": base_url,
                "timeout_ms": 120_000,
                "query_timeout_ms": 30_000,
            },
        });
        let response = crate::commands::configure::handle_configure(
            &request(json!({
                "id": "configure",
                "command": "configure",
                "harness": "opencode",
                "project_root": root,
                "storage_dir": storage,
                "config": [{"tier": "user", "source": "/u/aft.jsonc", "doc": doc.to_string()}],
            })),
            &ctx,
        );
        assert!(response.success, "configure failed: {response:?}");
        crate::runtime_drain::drain_deferred_configure_maintenance(&ctx);
        ctx
    };
    let session = parent_session(&parent);
    assert_eq!(session.children().len(), 2);

    let search = request(json!({
        "id": "parent-meaning",
        "command": "semantic_search",
        "query": "where does the helper give back one",
        "top_k": 10,
    }));
    let beta_gap = child_gap(
        "beta",
        Some(crate::data_layer::parent::NO_VECTORS.to_string()),
    );
    // Each child builds its checkout on first use; once both have
    // published, only beta is named, for its missing vectors.
    let reply = settled(
        || routed(&parent, &search).data,
        |reply| {
            let gaps = child_gaps(reply);
            gaps.contains(&beta_gap) && gaps.iter().all(|gap| gap["path"] == "beta")
        },
    );
    let files = reply["results"]
        .as_array()
        .unwrap_or_else(|| panic!("aft_search failed: {reply:#}"))
        .iter()
        .filter_map(|row| row["file"].as_str())
        .map(|file| file.replace('\\', "/"))
        .collect::<Vec<_>>();
    assert!(
        files.iter().any(|file| file.contains("alpha/src/")),
        "{reply:#}"
    );

    let children = parent.data_children().bound_for_test();
    let child = |name: &str| {
        children
            .iter()
            .find(|checkout| checkout.root() == root.join(name))
            .unwrap_or_else(|| panic!("{name} has no checkout"))
            .clone()
    };
    match child("alpha").meaning_status() {
        MeaningState::Serving { snapshot, .. } => {
            assert_eq!(snapshot.chunks(), alpha_fill.chunks);
            assert!(snapshot.coverage().complete(), "{:?}", snapshot.coverage());
        }
        state => panic!("alpha's plane does not serve: {state:?}"),
    }
    let beta = child("beta");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!beta.filling(), "a fill of beta was started");
    match beta.meaning_status() {
        MeaningState::Serving { snapshot, .. } => assert_eq!(snapshot.chunks(), 0),
        state => panic!("beta's plane is not read: {state:?}"),
    }
}
