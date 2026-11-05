//! Parent folders on the data layer, checked against today's answers.
//!
//! The fixture is a folder of repositories whose children published their
//! indexes through sessions of their own, as today's parent folder needs:
//! two with views on (their v3 lexical and call-graph views) and one with
//! views off (its legacy trigram artifact). The parent's answers are taken
//! with the data layer off, which is today's code, and then with it on,
//! from the children's own checkouts, and compared.

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
/// A line every published child holds twice.
const COMMON: &str = "PARENT_DATA_LAYER_COMMON_TOKEN";
/// A line each bare child holds once; no published child holds it.
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

fn drain(ctx: &AppContext) {
    crate::runtime_drain::drain_deferred_configure_maintenance(ctx);
    crate::runtime_drain::drain_watcher_events(ctx);
    crate::runtime_drain::drain_search_index_events(ctx);
}

/// Waits until a views-off child's trigram index is built and saved.
fn wait_trigram(ctx: &AppContext, root: &Path, storage: &Path) {
    let key = crate::search_index::artifact_cache_key(root);
    let artifact =
        crate::search_index::resolve_cache_dir_with_key(&key, Some(storage)).join("cache.bin");
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let ready = ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready);
        if ready && artifact.is_file() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child trigram index never saved: {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn callgraph_request(command: &str, file: &str) -> RawRequest {
    request(json!({
        "id": "parent-callgraph",
        "command": command,
        "file": file,
        "symbol": "target",
    }))
}

/// Asks a views-on child session for its call graph until it answers,
/// which publishes its v3 call-graph view.
fn publish_callgraph(ctx: &AppContext, root: &Path) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(ctx);
        let file = root.join("src/lib.rs");
        let answer = crate::commands::callers::handle_callers(
            &callgraph_request("callers", file.to_str().unwrap()),
            ctx,
        );
        if answer.success {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child call graph never answered: {}: {:?}",
            root.display(),
            answer.data
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// A call-graph command or inspect as the parent session answers it.
fn routed(ctx: &AppContext, req: &RawRequest) -> Response {
    crate::views::parent::route(req, ctx).expect("a parent folder answers this command itself")
}

/// The reply without what says which children it could not cover: the
/// `gaps`, and the note `attach_gaps` appends to the text.
fn covered(reply: &Value) -> Value {
    let mut reply = reply.clone();
    let object = reply.as_object_mut().unwrap();
    object.remove("gaps");
    if let Some(Value::String(text)) = object.get_mut("text") {
        if let Some(at) = text.find("\n\n(Parent folder: not covered by this answer:") {
            text.truncate(at);
        }
    }
    reply
}

/// Asserts two replies are equal, naming each top-level field that differs.
#[track_caller]
fn assert_same(data: &Value, today: &Value) {
    let (Some(data), Some(today)) = (data.as_object(), today.as_object()) else {
        assert_eq!(data, today);
        return;
    };
    let differing = data
        .keys()
        .chain(today.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|key| data.get(*key) != today.get(*key))
        .map(|key| {
            format!(
                "{key}: data layer {:?}, today {:?}",
                data.get(key),
                today.get(key)
            )
        })
        .collect::<Vec<_>>();
    assert!(differing.is_empty(), "{}", differing.join("\n"));
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

fn inspect_request() -> RawRequest {
    request(json!({
        "id": "parent-inspect",
        "command": "inspect",
        "sections": ["dead_code", "todos"],
    }))
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

/// A folder of 66 repositories: `alpha` and `beta` published with views on,
/// `gamma` with views off, `building` and `failed` never published, and 61
/// bare folders, the last two past the 64-repository cap. With the data
/// layer on, the parent answers grep, glob, the call graph and inspect for
/// the published children exactly as today, and also for the children
/// today cannot answer for, with today's named gaps for a child still
/// building, a child that failed, the children past the cap and the files
/// outside every child. With the data layer off, it binds no checkout.
#[test]
fn a_parent_folder_answers_from_its_childrens_checkouts_as_from_their_published_indexes() {
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

    let (children, parent) = {
        let _git = crate::test_env::hermetic_git_env_guard();
        let mut children = Vec::new();
        for name in ["alpha", "beta"] {
            let child = root.join(name);
            let ctx = configure(&child, &storage, true, true);
            publish_callgraph(&ctx, &child);
            children.push(ctx);
        }
        let gamma = root.join("gamma");
        let ctx = configure(&gamma, &storage, false, false);
        wait_trigram(&ctx, &gamma, &storage);
        children.push(ctx);
        (children, configure(&root, &storage, true, true))
    };
    let session = parent_session(&parent);
    assert_eq!(session.children().len(), 64);
    let child = |name: &str| {
        session
            .children()
            .into_iter()
            .find(|child| child.root == root.join(name))
            .unwrap()
    };
    settled(
        || {
            ["alpha", "beta", "gamma"]
                .iter()
                .all(|name| child(name).trigram_ready())
                && ["alpha", "beta"]
                    .iter()
                    .all(|name| child(name).callgraph_ready())
        },
        |ready| *ready,
    );

    // Today's answers.
    let alpha = "alpha/src/lib.rs";
    let today_grep = grep(&parent, COMMON);
    assert_eq!(matched_files(&today_grep).len(), 6, "{today_grep:#}");
    let today_bare = grep(&parent, BARE);
    assert!(matched_files(&today_bare).is_empty(), "{today_bare:#}");
    let today_glob = glob(&parent, "**/*.rs");
    let today_callers = routed(&parent, &callgraph_request("callers", alpha));
    let today_tree = routed(&parent, &callgraph_request("call_tree", alpha));
    let today_building = routed(
        &parent,
        &callgraph_request("callers", "building/src/lib.rs"),
    );
    let today_capped = routed(&parent, &callgraph_request("callers", "zz-60/notes.txt"));
    let today_inspect = routed(&parent, &inspect_request());
    assert!(today_callers.success, "{:?}", today_callers.data);
    // With the data layer off, nothing of it was bound or written.
    assert!(!storage.join("data").exists());
    assert!(parent.data_children().bound_for_test().is_empty());
    let scope_gaps = gaps(&today_grep)
        .into_iter()
        .filter(|gap| gap["kind"] != "parent_child_unavailable")
        .collect::<Vec<_>>();
    assert_eq!(
        scope_gaps
            .iter()
            .map(|gap| (gap["kind"].as_str().unwrap(), gap["path"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("parent_children_over_cap", "."),
            ("outside_child_repositories", "README.md")
        ],
        "{today_grep:#}"
    );

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
    let failure = "checkout scan: the fixture's build failed".to_string();

    crate::data_layer::runtime::force_on_for_test();
    let named = vec![
        child_gap("building", Plane::<()>::Loading.gap_reason("trigram")),
        child_gap(
            "failed",
            Plane::<()>::Gap(failure.clone()).gap_reason("trigram"),
        ),
    ];
    // Every other child builds its checkout on first use.
    let data_grep = settled(|| grep(&parent, COMMON), |reply| child_gaps(reply) == named);
    assert_same(&covered(&data_grep), &covered(&today_grep));
    assert_eq!(gaps(&data_grep), [named.clone(), scope_gaps].concat());
    // The bare children today cannot answer for, past the cap excepted.
    let data_bare = grep(&parent, BARE);
    let mut bare = matched_files(&data_bare)
        .into_iter()
        .map(|file| {
            file.strip_prefix(&format!("{}/", root.display()))
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    bare.sort();
    assert_eq!(
        bare,
        (0..59)
            .map(|index| format!("zz-{index:02}/notes.txt"))
            .collect::<Vec<_>>()
    );
    let data_glob = glob(&parent, "**/*.rs");
    assert_same(&covered(&data_glob), &covered(&today_glob));
    assert_eq!(child_gaps(&data_glob), named);

    // The call graph, answered as today but current, so without `as_of`.
    for (command, today) in [("callers", &today_callers), ("call_tree", &today_tree)] {
        let answer = settled(
            || routed(&parent, &callgraph_request(command, alpha)),
            |answer| answer.success,
        );
        let mut expected = today.data.clone();
        expected.as_object_mut().unwrap().remove("as_of");
        assert_same(&answer.data, &expected);
    }
    let building = routed(
        &parent,
        &callgraph_request("callers", "building/src/lib.rs"),
    );
    assert_eq!(building.data["code"], today_building.data["code"]);
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
    let capped = routed(&parent, &callgraph_request("callers", "zz-60/notes.txt"));
    assert_same(&capped.data, &today_capped.data);
    // Inspect reads what the children's own sessions stored, as today.
    assert_same(
        &routed(&parent, &inspect_request()).data,
        &today_inspect.data,
    );

    // The parent folder's own root binds nothing; each served child has
    // its own checkout.
    assert!(parent.data_checkout().is_none());
    assert_eq!(parent.data_children().bound_for_test().len(), 64);
    drop(children);
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

    crate::data_layer::runtime::force_on_for_test();
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
