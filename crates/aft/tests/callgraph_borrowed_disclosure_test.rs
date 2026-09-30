//! A checkout that borrows another checkout's callgraph must say so whenever
//! the borrowed graph can differ from its own files.
//!
//! The owner repository is on a commit where `target` is called by
//! `newCaller`. A linked worktree at the previous commit, where `target` is
//! called by `oldCaller` and `legacyHelper` still exists, reads the owner's
//! graph: its answers must be marked incomplete and name both commits. A
//! linked worktree at the owner's commit with no edits of its own reads a
//! graph that fits it, so its answers stay unmarked, as do the owner's.

#[path = "helpers/mod.rs"]
mod test_helpers;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use serde_json::{json, Value};

const READY_DEADLINE: Duration = Duration::from_secs(60);

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    test_helpers::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Two commits: `old` has `oldCaller` calling `target` and a `legacyHelper`;
/// `new` replaces the caller with `newCaller` and drops `legacyHelper`.
/// Returns the repository root and both commit ids.
fn repository(parent: &Path) -> (PathBuf, String, String) {
    let root = parent.join("owner");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "borrowed@example.test"]);
    git(&root, &["config", "user.name", "Borrowed Callgraph Test"]);

    write(
        &root,
        "src/target.ts",
        "export function target() { return 1; }\nexport function legacyHelper() { return 2; }\n",
    );
    write(
        &root,
        "src/old_caller.ts",
        "import { target } from './target';\nexport function oldCaller() { return target(); }\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "old callers"]);
    let old = git(&root, &["rev-parse", "HEAD"]);

    write(
        &root,
        "src/target.ts",
        "export function target() { return 1; }\n",
    );
    std::fs::remove_file(root.join("src/old_caller.ts")).unwrap();
    write(
        &root,
        "src/new_caller.ts",
        "import { target } from './target';\nexport function newCaller() { return target(); }\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "new callers"]);
    let new = git(&root, &["rev-parse", "HEAD"]);
    (root, old, new)
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).expect("valid request")
}

fn configure(root: &Path, storage: &Path) -> Arc<AppContext> {
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ));
    let configured = aft::commands::configure::handle_configure(
        &request(json!({
            "id": "configure-borrowed-callgraph",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": test_helpers::user_config(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": true,
                "worktree": { "ram_overlay": true }
            }))
        })),
        &ctx,
    );
    assert!(configured.success, "configure failed: {configured:?}");
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    wait_until_ready(&ctx);
    ctx
}

/// Wait until the callgraph answers and the search index is installed. In a
/// worktree the search index is the owner's snapshot with this checkout's own
/// differences folded into a RAM overlay; the disclosure reads its count of
/// differing files, so asserting before it is installed would see "unknown".
fn wait_until_ready(ctx: &AppContext) {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        aft::runtime_drain::drain_watcher_events(ctx);
        aft::runtime_drain::drain_search_index_events(ctx);
        aft::runtime_drain::drain_callgraph_store_events(ctx);
        let callgraph_ready = match ctx.callgraph_store_for_ops() {
            CallgraphStoreAccess::Ready(_) => true,
            CallgraphStoreAccess::Building => false,
            CallgraphStoreAccess::Suspended(reason) => panic!("callgraph suspended: {reason:?}"),
            CallgraphStoreAccess::Unavailable => false,
            CallgraphStoreAccess::Off => panic!("callgraph index off"),
            CallgraphStoreAccess::Error(error) => panic!("callgraph failed: {error}"),
        };
        let search_ready = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|index| index.ready);
        if callgraph_ready && search_ready {
            return;
        }
        assert!(Instant::now() < deadline, "indexes did not become ready");
        thread::sleep(Duration::from_millis(10));
    }
}

fn callers(ctx: &AppContext, root: &Path, symbol: &str) -> Response {
    aft::commands::callers::handle_callers(
        &request(json!({
            "id": "borrowed-callers",
            "command": "callers",
            "file": root.join("src/target.ts"),
            "symbol": symbol,
            "depth": 1
        })),
        ctx,
    )
}

fn rendered(response: &Response) -> String {
    aft::subc_format::format_callgraph("callers", &response.data, false)
}

/// The owner's graph can report ready before its last edges are written, so
/// ask again until the caller list is populated.
fn populated_callers(ctx: &AppContext, root: &Path, symbol: &str) -> Response {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let response = callers(ctx, root, symbol);
        if response.success && !caller_symbols(&response).is_empty() {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "callers of {symbol} never populated: {response:?}"
        );
        aft::runtime_drain::drain_callgraph_store_events(ctx);
        thread::sleep(Duration::from_millis(25));
    }
}

fn caller_symbols(response: &Response) -> Vec<String> {
    response.data["callers"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["callers"].as_array().cloned().unwrap_or_default())
        .filter_map(|caller| caller["symbol"].as_str().map(str::to_string))
        .collect()
}

fn assert_unmarked(response: &Response, who: &str) {
    assert!(response.success, "{who}: callers failed: {response:?}");
    assert!(
        response.data.get("complete").is_none(),
        "{who}: answer was marked incomplete: {:#}",
        response.data
    );
    assert!(
        response.data.get("borrowed_callgraph").is_none(),
        "{who}: answer carried a borrowed-graph disclosure: {:#}",
        response.data
    );
    assert!(
        !rendered(response).contains("borrowed"),
        "{who}: rendered text mentions borrowing: {}",
        rendered(response)
    );
}

#[test]
fn borrowed_callgraph_answers_disclose_a_checkout_mismatch_and_only_then() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let (owner_root, old_commit, new_commit) = repository(temp.path());

    let owner = configure(&owner_root, &storage);
    let owner_answer = populated_callers(&owner, &owner_root, "target");
    assert_eq!(caller_symbols(&owner_answer), vec!["newCaller".to_string()]);
    assert_unmarked(&owner_answer, "owner");

    // A worktree on the older commit reads the owner's graph, whose callers
    // and symbols belong to the newer commit.
    let old_worktree = temp.path().join("old-worktree");
    git(
        &owner_root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            old_worktree.to_str().unwrap(),
            &old_commit,
        ],
    );
    let old_worktree = std::fs::canonicalize(old_worktree).unwrap();
    let old = configure(&old_worktree, &storage);

    let answer = populated_callers(&old, &old_worktree, "target");
    assert!(answer.success, "callers failed: {answer:?}");
    assert_eq!(answer.data["complete"], json!(false), "{:#}", answer.data);
    let borrowed = &answer.data["borrowed_callgraph"];
    assert_eq!(
        borrowed["owner_checkout"],
        json!(owner_root.display().to_string())
    );
    assert_eq!(borrowed["owner_head"], json!(new_commit));
    assert_eq!(borrowed["checkout_head"], json!(old_commit));
    let text = rendered(&answer);
    let first_line = text.lines().next().unwrap_or_default();
    assert_eq!(
        first_line,
        format!(
            "callgraph: borrowed from {} at {}; this checkout is at {} with {}, so callers and line numbers may not match this tree",
            owner_root.display(),
            &new_commit[..7],
            &old_commit[..7],
            match borrowed["changed_files"].as_u64() {
                Some(1) => "1 changed file".to_string(),
                Some(count) => format!("{count} changed files"),
                None => "an unknown number of changed files".to_string(),
            }
        ),
        "rendered: {text}"
    );
    // The overlay has compared the owner's snapshot with this checkout:
    // old_caller.ts and target.ts differ, new_caller.ts is missing here.
    assert_eq!(borrowed["changed_files"], json!(3), "{:#}", answer.data);

    let missing = callers(&old, &old_worktree, "legacyHelper");
    assert!(!missing.success, "legacyHelper resolved: {missing:?}");
    assert_eq!(missing.data["code"], "symbol_not_found");
    let message = missing.data["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("may exist in this checkout but not in the borrowed graph"),
        "not-found message lacks the borrowed-graph hint: {message}"
    );
    assert!(message.contains(&old_commit[..7]) && message.contains(&new_commit[..7]));
    assert!(!message.contains('\n'));

    // A worktree on the owner's commit with no edits of its own reads a graph
    // that describes it exactly.
    let same_worktree = temp.path().join("same-worktree");
    git(
        &owner_root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            same_worktree.to_str().unwrap(),
            &new_commit,
        ],
    );
    let same_worktree = std::fs::canonicalize(same_worktree).unwrap();
    let same = configure(&same_worktree, &storage);
    let same_answer = populated_callers(&same, &same_worktree, "target");
    assert_eq!(caller_symbols(&same_answer), vec!["newCaller".to_string()]);
    assert_unmarked(&same_answer, "same-commit worktree");
}
