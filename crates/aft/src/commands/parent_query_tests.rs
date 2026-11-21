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
use crate::parent_search::Plane;
use crate::protocol::{RawRequest, Response};

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

/// A session bound to `root` through configure, with the call graph on or off.
fn configure(root: &Path, storage: &Path, callgraph: bool) -> Arc<AppContext> {
    let ctx = Arc::new(crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        },
    ));
    let doc = json!({
        "indexes": {"trigram": true, "semantic": false, "callgraph": callgraph},
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

/// A command as the parent session answers it.
fn routed(ctx: &AppContext, req: &RawRequest) -> Response {
    crate::commands::parent_query::route(req, ctx)
        .expect("a parent folder answers this command itself")
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
    crate::context::Executor,
    Arc<AppContext>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = Arc::new(crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    ));
    ctx.mark_subc_bound();
    let executor = crate::context::Executor::new();
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
fn parent_session(ctx: &AppContext) -> Arc<crate::parent_search::ParentSession> {
    let deadline = Instant::now() + DEADLINE;
    let session = loop {
        crate::runtime_drain::drain_deferred_configure_maintenance(ctx);
        if let Some(session) = crate::parent_search::session_for(ctx) {
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

/// Two ready repositories, then beta's checkout replaced by the existing
/// refusing-executor fixture so it stays building without a timing race.
fn with_loading_child(run: impl FnOnce(&AppContext, &Path, &Path)) {
    with_prepared_loading_child(|_| {}, run);
}

/// [`with_loading_child`], with `prepare` run on the parent folder once both
/// repositories are committed and before anything indexes them.
fn with_prepared_loading_child(
    prepare: impl FnOnce(&Path),
    run: impl FnOnce(&AppContext, &Path, &Path),
) {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["alpha", "beta"] {
        init_repo(&root.join(name), name);
    }
    prepare(&root);
    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        configure(&root, &storage, false)
    };
    let session = parent_session(&parent);
    session.pause_refresh(true);
    settled(
        || grep(&parent, COMMON),
        |reply| reply["index_status"] == "Ready" && matched_files(reply).len() == 4,
    );
    let (refusing, _executor, _unbound, _dir) = refusing_executor();
    let held_storage = base.join("held-storage");
    std::fs::create_dir_all(&held_storage).unwrap();
    let held = DataCheckout::open(&root.join("beta"), &held_storage, Some(refusing)).unwrap();
    parent
        .data_children()
        .insert_for_test(session.root(), &root.join("beta"), held);
    run(&parent, &root, &storage);
}

#[test]
fn parent_loading_child_walks_until_ready_then_returns_to_indexed_grep_and_glob() {
    with_loading_child(|parent, root, storage| {
        let expected = [
            "alpha/src/lib.rs",
            "alpha/src/util.rs",
            "beta/src/lib.rs",
            "beta/src/util.rs",
        ];
        let loading = grep(parent, COMMON);
        assert_eq!(
            relative_files(root, matched_files(&loading)),
            expected,
            "{loading:#}"
        );
        assert_eq!(loading["complete"], true, "{loading:#}");
        assert!(gaps(&loading).is_empty(), "{loading:#}");
        assert_eq!(loading["index_status"], "Building");
        assert_eq!(loading["fallback"], "filesystem");
        assert_eq!(loading["files_read_directly"], 4);
        assert!(
            loading["text"]
                .as_str()
                .unwrap()
                .contains("[index: building; searched all 4 files directly]"),
            "{loading:#}"
        );

        let loading_glob = glob(parent, "**/*.rs");
        let files = |reply: &Value| {
            relative_files(
                root,
                reply["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|file| file.as_str().unwrap().replace('\\', "/"))
                    .collect(),
            )
        };
        assert_eq!(files(&loading_glob), expected, "{loading_glob:#}");
        assert_eq!(loading_glob["complete"], true, "{loading_glob:#}");
        assert_eq!(loading_glob["fallback"], "filesystem");
        assert!(gaps(&loading_glob).is_empty());
        assert_eq!(
            files(&glob(parent, "beta/src/*.rs")),
            ["beta/src/lib.rs", "beta/src/util.rs"]
        );

        // Restoring a normally scheduled checkout must restore the ready path,
        // not keep walking beta after its text view is available.
        let ready = DataCheckout::open(&root.join("beta"), storage, None).unwrap();
        parent
            .data_children()
            .insert_for_test(root, &root.join("beta"), ready);
        let indexed = settled(
            || grep(parent, COMMON),
            |reply| reply["index_status"] == "Ready",
        );
        assert_eq!(
            relative_files(root, matched_files(&indexed)),
            expected,
            "{indexed:#}"
        );
        assert_eq!(indexed["complete"], true);
        assert_eq!(indexed["files_read_directly"], 0);
        assert!(indexed["fallback"].is_null());
        assert!(gaps(&indexed).is_empty());
        assert!(!indexed["text"].as_str().unwrap().contains("[index:"));
        crate::search_index::audit_work_reset();
        let indexed_glob = glob(parent, "**/*.rs");
        assert_eq!(files(&indexed_glob), expected, "{indexed_glob:#}");
        assert_eq!(indexed_glob["complete"], true);
        assert!(indexed_glob["fallback"].is_null());
        assert!(gaps(&indexed_glob).is_empty());
        let work = crate::search_index::audit_work();
        assert_eq!(
            work.sort_stats, 0,
            "warm glob should sort with indexed times"
        );
        assert_eq!(work.sort_canonicalizes, 0);
    });
}

/// A ready child's unreadable folder is named in a parent glob even when
/// another child, still building, is walked instead: walking one child says
/// nothing about the folders the ready child's index skipped.
#[cfg(target_os = "linux")]
#[test]
fn parent_glob_names_a_ready_childs_unreadable_folder_beside_a_walked_child() {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    with_prepared_loading_child(
        |root| {
            let locked = root.join("alpha/locked");
            write(&locked, "hidden.rs", "pub fn hidden() {}\n");
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0)).unwrap();
            let restore_on_panic = Restore(locked.clone());
            assert_eq!(
                std::fs::read_dir(&locked)
                    .err()
                    .expect("run without DAC override privileges")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            // Stays unreadable while the parent indexes it; the run below
            // makes it readable again.
            std::mem::forget(restore_on_panic);
        },
        |parent, root, _| {
            // Readable again before the fixture removes its folder.
            let _restore = Restore(root.join("alpha/locked"));
            let reply = glob(parent, "**/*.rs");
            assert_eq!(reply["fallback"], "filesystem", "{reply:#}");
            assert_eq!(reply["complete"], false, "{reply:#}");
            assert_eq!(reply["skipped_folders"], 1, "{reply:#}");
            let unreadable = gaps(&reply)
                .into_iter()
                .filter(|gap| gap["kind"] == "unreadable_folder")
                .collect::<Vec<_>>();
            assert_eq!(unreadable.len(), 1, "{reply:#}");
            assert_eq!(
                unreadable[0]["path"],
                json!(root.join("alpha/locked")),
                "{reply:#}"
            );
            assert!(
                reply["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("(Index skipped 1 unreadable folder;"),
                "{reply:#}"
            );
        },
    );
}

fn assert_gap_leads(reply: &Value, child: &str) {
    assert_eq!(reply["complete"], false, "{reply:#}");
    let gaps = child_gaps(reply);
    assert_eq!(gaps.len(), 1, "{reply:#}");
    assert_eq!(gaps[0]["path"], child, "{reply:#}");
    let block = format!(
        "(Parent folder: not covered by this answer:\n- {child}: {})\n\n",
        gaps[0]["reason"].as_str().unwrap()
    );
    assert!(
        reply["text"].as_str().unwrap().starts_with(&block),
        "{reply:#}"
    );
}

#[test]
fn parent_loading_child_failed_walk_is_a_gap_before_zero_matches() {
    with_loading_child(|parent, root, _| {
        std::fs::remove_dir_all(root.join("beta")).unwrap();
        let reply = grep(parent, "TOKEN_NOT_IN_EITHER_REPOSITORY");
        assert_gap_leads(&reply, "beta");
        assert_eq!(reply["total_matches"], 0);
        assert!(!reply["text"]
            .as_str()
            .unwrap()
            .starts_with("Found 0 matches"));
        assert!(!child_gaps(&reply)[0]["reason"]
            .as_str()
            .unwrap()
            .contains("still loading"));
        assert_gap_leads(&glob(parent, "**/*.missing"), "beta");
    });
}

#[test]
fn parent_loading_child_bounded_walk_keeps_matches_and_reports_a_gap() {
    with_loading_child(|parent, _, _| {
        crate::grep_executor::with_walk_limit(0, || {
            let reply = grep(parent, COMMON);
            assert_gap_leads(&reply, "beta");
            assert_eq!(matched_files(&reply).len(), 2, "{reply:#}");
            assert_eq!(reply["walk_stopped_by"], "file_limit");
            assert!(child_gaps(&reply)[0]["reason"]
                .as_str()
                .unwrap()
                .contains("fallback walk stops at 0 files"));
            let glob_reply = glob(parent, "**/*.rs");
            assert_gap_leads(&glob_reply, "beta");
            assert_eq!(glob_reply["files"].as_array().unwrap().len(), 2);
            assert_eq!(glob_reply["walk_truncated"], true);
        });
    });
}

/// A folder of 66 repositories: `alpha`, `beta` and `gamma` committed
/// crates, `building` and `failed` crates whose checkouts never finish a
/// build, and 61 bare folders, the last two past the 64-repository cap. With
/// the data layer on, the parent answers grep, glob and the call graph from
/// each served child's own checkout, built on first use. Grep and glob walk
/// the building child; failed children, children past the cap and files outside
/// every child remain named gaps.
#[test]
fn a_parent_folder_answers_from_its_childrens_checkouts() {
    crate::parent_search::set_default_refresh_interval(Duration::from_millis(100));
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
        configure(&root, &storage, true)
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

    let named = vec![child_gap(
        "failed",
        Plane::<()>::Gap(failure.clone()).gap_reason("trigram"),
    )];
    // Every other child builds its checkout on first use.
    let data_grep = settled(|| grep(&parent, COMMON), |reply| child_gaps(reply) == named);
    // `init_repo` writes COMMON once in each crate's src/lib.rs and once in
    // its src/util.rs; `building` answers by walking, but `failed` cannot answer.
    assert_eq!(
        relative_files(&root, matched_files(&data_grep)),
        [
            "alpha/src/lib.rs",
            "alpha/src/util.rs",
            "beta/src/lib.rs",
            "beta/src/util.rs",
            "building/src/lib.rs",
            "building/src/util.rs",
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
            "building/src/lib.rs",
            "building/src/util.rs",
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
/// With the data layer on, a parent folder's TODO count sums the markers its
/// children's checkouts hold, with paths relative to the folder. A child
/// repository with no checkout bound yet is not counted silently as zero:
/// the answer is partial, and its gap names the repository.
#[test]
fn a_parent_folders_todo_count_names_the_repositories_not_indexed_yet() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["alpha", "beta"] {
        init_repo(&root.join(name), name);
        write(
            &root.join(name),
            "src/todo.rs",
            &format!("// TODO: in {name}\n"),
        );
    }
    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        configure(&root, &storage, true)
    };
    let session = parent_session(&parent);
    assert_eq!(session.children().len(), 2);
    // Only `alpha` has a checkout, built and published.
    let alpha = DataCheckout::open(&root.join("alpha"), &storage, None).unwrap();
    parent
        .data_children()
        .insert_for_test(session.root(), &root.join("alpha"), Arc::clone(&alpha));
    alpha.request_refresh();
    settled(
        || {
            alpha
                .view()
                .is_some_and(|view| view.status().callgraph_published)
                && !alpha.checkout().status().building
        },
        |published| *published,
    );

    let scope = crate::inspect::inventory::InspectInventory::collect(&root, || true).sources;
    let mut job = crate::inspect::InspectJob {
        job_id: 1,
        key: crate::inspect::JobKey::for_category_scope(
            crate::inspect_category::InspectCategory::Todos,
            &crate::inspect::JobScope::for_project(root.clone()),
        ),
        category: crate::inspect_category::InspectCategory::Todos,
        scope_files: scope,
        project_root: root.clone(),
        inspect_dir: base.join("inspect"),
        config: parent.config(),
        symbol_cache: Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: None,
    };
    let aggregate = crate::inspect::scanners::todos::run_todos_scan(&job)
        .outcome
        .unwrap()
        .aggregate;
    assert_eq!(aggregate["count"], 1, "{aggregate:#}");
    assert_eq!(aggregate["items"][0]["file"], "alpha/src/todo.rs");
    assert_eq!(aggregate["items"][0]["text"], "in alpha");
    assert_eq!(aggregate["complete"], false, "{aggregate:#}");
    assert_eq!(
        aggregate["gaps"],
        json!([{
            "kind": "analysis_incomplete",
            "reason": "repositories in this folder not indexed yet: beta",
            "repositories": ["beta"],
        }]),
        "{aggregate:#}"
    );
    // The repository's files are covered by its gap, not listed one by one.
    assert!(aggregate.get("skipped_files").is_none(), "{aggregate:#}");
    assert_eq!(
        serde_json::to_vec(&aggregate).unwrap(),
        serde_json::to_vec(&crate::inspect::scanners::todos::uncached_aggregate_for_test(&job))
            .unwrap(),
    );
    // Once both repositories are indexed, detail order follows the caller's
    // scope rather than the order in which child checkouts are consulted.
    let beta = DataCheckout::open(&root.join("beta"), &storage, None).unwrap();
    parent
        .data_children()
        .insert_for_test(session.root(), &root.join("beta"), Arc::clone(&beta));
    beta.request_refresh();
    settled(
        || {
            beta.view()
                .is_some_and(|view| view.status().callgraph_published)
                && !beta.checkout().status().building
        },
        |published| *published,
    );
    job.scope_files.reverse();
    let aggregate = crate::inspect::scanners::todos::run_todos_scan(&job)
        .outcome
        .unwrap()
        .aggregate;
    assert_eq!(aggregate["count"], 2);
    assert_eq!(aggregate["items"][0]["file"], "beta/src/todo.rs");
    assert_eq!(
        serde_json::to_vec(&aggregate).unwrap(),
        serde_json::to_vec(&crate::inspect::scanners::todos::uncached_aggregate_for_test(&job),)
            .unwrap()
    );
}

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
        configure(&root, &storage, true)
    };
    let _session = parent_session(&parent);
    parent.mark_subc_bound();
    let executor = crate::context::Executor::new();
    let root_id = crate::path_identity::ProjectRootId::from_path(&root).unwrap();
    assert!(executor.register_actor(root_id, Arc::clone(&parent)));
    parent.mark_subc_unbound();

    let loading = grep(&parent, COMMON);
    assert!(child_gaps(&loading).is_empty(), "{loading:#}");
    assert_eq!(matched_files(&loading).len(), 4, "{loading:#}");
    assert_eq!(loading["index_status"], "Building", "{loading:#}");
    std::thread::sleep(Duration::from_millis(500));
    let bound = parent.data_children().bound_for_test();
    assert_eq!(bound.len(), 2);
    assert!(
        bound.iter().all(|checkout| checkout.view().is_none()),
        "a child of an unbound parent root was built"
    );
    let still_loading = grep(&parent, COMMON);
    assert!(child_gaps(&still_loading).is_empty(), "{still_loading:#}");
    assert_eq!(still_loading["index_status"], "Building");

    parent.mark_subc_bound();
    let served = settled(
        || grep(&parent, COMMON),
        |reply| reply["index_status"] == "Ready" && matched_files(reply).len() == 4,
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

#[test]
fn audit1432_parent_search_collects_ceiling_skips_from_both_children() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["alpha", "beta"] {
        init_repo(&root.join(name), name);
        let mut bytes = vec![b'x'; 17 << 20];
        bytes.extend_from_slice(b"only_at_tail");
        std::fs::write(root.join(name).join("large.rs"), bytes).unwrap();
    }
    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        configure(&root, &storage, false)
    };
    let session = parent_session(&parent);
    assert_eq!(session.children().len(), 2);
    let req = request(
        json!({"id":"parent-skips", "command":"semantic_search", "query":"(?s)only_at_tail.*", "include_tests":true, "top_k":10}),
    );
    let response = settled(
        || routed(&parent, &req),
        |response| {
            response.data["skipped_files"]
                .as_array()
                .is_some_and(|skips| skips.len() == 2)
        },
    );
    assert!(response.success, "{response:?}");
    let skipped = response.data["skipped_files"]
        .as_array()
        .unwrap_or_else(|| panic!("{response:?}"));
    for name in ["alpha", "beta"] {
        assert!(
            skipped.iter().any(
                |skip| skip["file"] == json!(root.join(name).join("large.rs"))
                    && skip["reason"] == crate::large_read::WHOLE_READ_SKIP_REASON
            ),
            "{response:?}"
        );
    }
    assert_eq!(skipped.len(), 2, "{response:?}");
    assert_eq!(response.data["complete"], false);
    assert_eq!(response.data["skipped_files_remaining"], 0);
}

#[test]
fn parent_session_search_accepts_pattern_only_and_split_requests() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    for (repo, name) in [(&alpha, "alpha"), (&beta, "beta")] {
        init_repo(repo, name);
    }
    write(
        &alpha,
        "src/mention.rs",
        "fn run() {\n    cross_parent();\n}\n",
    );
    write(
        &alpha,
        "src/queue.rs",
        "// The scheduler drains the work queue.\nfn drain_queue() {}\n",
    );
    write(&beta, "src/definition.rs", "pub fn cross_parent() {}\n");
    for repo in [&alpha, &beta] {
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "search fixtures"]);
    }
    let parent = {
        let _git = crate::test_env::hermetic_git_env_guard();
        configure(&root, &storage, false)
    };
    assert_eq!(parent_session(&parent).children().len(), 2);
    settled(
        || grep(&parent, "cross_parent"),
        |reply| reply["complete"] == true && matched_files(reply).len() == 2,
    );

    let search = |params: Value| {
        let mut req = request(json!({"id":"parent-search", "command":"semantic_search"}));
        req.params = params;
        routed(&parent, &req)
    };
    let files = |reply: &Value| {
        reply["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply:#}"))
            .iter()
            .map(|row| row["file"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let mention = "alpha/src/mention.rs";
    let definition = "beta/src/definition.rs";
    let pattern = search(json!({"pattern":"cross_parent"}));
    assert!(pattern.success, "{pattern:?}");
    assert_eq!(files(&pattern.data), [definition, mention]);
    assert!(pattern.data["text"].as_str().unwrap().starts_with(&format!(
        "{definition}\n1: pub fn cross_parent() {{}}\n\n{mention}\n2:     cross_parent();\n\n\
         2 files match (2 matching lines); one result per file, definitions first.\n[interpreted_as: regex]"
    )), "{}", pattern.data["text"]);

    let page = search(json!({"pattern":"cross_parent", "offset":1, "topK":1}));
    assert!(page.success, "{page:?}");
    assert_eq!(files(&page.data), [mention]);
    assert!(page.data["text"]
        .as_str()
        .unwrap()
        .contains("2 files match (2 matching lines)"));

    let query = search(json!({"query":r"\bcross_parent\b"}));
    assert!(query.success, "{query:?}");
    assert_eq!(
        query.data["text"],
        format!(
        "{mention}:2-2 \n  cross_parent();\n{definition}:1-1 \n  pub fn cross_parent() {{}}\n\n\
         (Parent folder: not covered by this answer:\n- .: semantic: semantic search is off)"
    )
    );

    let prose = "scheduler drains the work queue";
    let prose_only = search(json!({"query":prose}));
    assert!(!files(&prose_only.data)
        .iter()
        .any(|file| file == definition));
    let split = search(json!({"query":prose, "pattern":"cross_parent"}));
    assert!(split.success, "{split:?}");
    assert_eq!(split.data["interpreted_as"], "split");
    assert!(
        split.data["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["file"] == definition && row["matched_by"] == "pattern" }),
        "{}",
        split.data
    );
    assert!(split.data["text"].as_str().unwrap().starts_with(&format!(
        "[pattern `cross_parent`: 2 files matched, 0 also found by the query; defined in {definition}:1]\n\n"
    )), "{}", split.data["text"]);

    let empty = search(json!({}));
    let req = request(json!({"id":"parent-search", "command":"semantic_search"}));
    let ordinary = crate::commands::semantic_search::handle_semantic_search(&req, &parent);
    assert!(!empty.success);
    assert_eq!(empty.data, ordinary.data);
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
    crate::parent_search::set_default_refresh_interval(Duration::from_millis(100));
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    for name in ["alpha", "beta"] {
        init_repo(&root.join(name), name);
    }
    let (base_url, gate) = crate::data_layer::meaning_query::test_support::serve("never-held");
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
        let ctx = Arc::new(crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..crate::config::Config::default()
            },
        ));
        let doc = json!({
            "indexes": {"trigram": true, "semantic": true, "callgraph": false},
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

/// A bind holds its child's slot for its whole scan. A watcher batch for
/// another child, already bound, must be recorded without waiting for it.
#[test]
fn a_child_bind_in_progress_does_not_hold_up_another_childs_watcher_paths() {
    use super::{bind, ChildCheckouts};
    use crate::data_layer::checkout::Planes;
    use crate::data_layer::runtime::Fresh;

    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let parent = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let binding = parent.join("binding");
    let bound = parent.join("bound");
    write(&binding, "src/lib.rs", "pub fn binding() {}\n");
    write(&bound, "src/lib.rs", "pub fn bound() {}\n");
    let children = Arc::new(ChildCheckouts::default());
    let bound_slot = children.slot(&parent, &bound);
    let checkout = bind(&bound_slot, &bound, &storage, None, Planes::ALL).unwrap();
    assert!(matches!(
        checkout.fresh_view(Duration::from_secs(60)),
        Fresh::Ready(_)
    ));
    assert_eq!(checkout.checkout().changed_paths(), 0);

    let binding_slot = children.slot(&parent, &binding);
    let bind_in_progress = binding_slot.lock();
    let edited = write(&bound, "src/added.rs", "pub fn added() {}\n");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let noter = {
        let children = Arc::clone(&children);
        std::thread::spawn(move || {
            children.note_paths(&[edited]);
            done_tx.send(()).unwrap();
        })
    };
    // A bound on failure only: a passing run is answered at once.
    let finished = done_rx.recv_timeout(Duration::from_secs(30));
    drop(bind_in_progress);
    noter.join().unwrap();
    assert!(
        finished.is_ok(),
        "a watcher path for a bound child waited for another child's bind"
    );
    assert_eq!(checkout.checkout().changed_paths(), 1);
}
