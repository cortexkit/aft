//! A checkout that borrows another checkout's callgraph must say so whenever
//! the borrowed graph can differ from its own files, and must count only the
//! files that really differ.
//!
//! The owner repository moves to a newer commit while it runs, so the shared
//! search snapshot it wrote at startup no longer describes it; the callgraph
//! does. A linked worktree at an older commit reads the owner's graph: its
//! answers are marked incomplete, name both commits and count the files that
//! differ between the commits. A linked worktree at the owner's commit with no
//! edits on either side reads a graph that fits it, so its answers stay
//! unmarked, as do the owner's. Edits in a worktree are counted and located in
//! that worktree; uncommitted edits in the owner are counted and located in
//! the borrowed checkout.

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
/// `new` replaces the caller with `newCaller` and drops `legacyHelper`. Both
/// carry `util.ts`, which never changes, and a binary file, which the search
/// index holds without hashing. Returns the repository root and both commit
/// ids.
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
    write(
        &root,
        "src/util.ts",
        "import { target } from './target';\nexport function util() { return 3; }\n",
    );
    let blob = root.join("assets/blob.bin");
    std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
    std::fs::write(blob, b"\0\x01\x02binary\0").unwrap();
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

/// Append a function calling `target` to a source file.
fn append_caller(root: &Path, relative: &str, caller: &str) {
    let path = root.join(relative);
    let mut contents = std::fs::read_to_string(&path).unwrap();
    contents.push_str(&format!(
        "export function {caller}() {{ return target(); }}\n"
    ));
    std::fs::write(path, contents).unwrap();
}

fn linked_worktree(owner_root: &Path, path: &Path, commit: &str) -> PathBuf {
    git(
        owner_root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            path.to_str().unwrap(),
            commit,
        ],
    );
    std::fs::canonicalize(path).unwrap()
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).expect("valid request")
}

fn configure(root: &Path, storage: &Path) -> Arc<AppContext> {
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
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

/// Ask until the callers include every name in `expected`, applying watcher
/// events between attempts so edits on disk reach the graph.
fn callers_including(ctx: &AppContext, root: &Path, expected: &[&str]) -> Response {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let response = callers(ctx, root, "target");
        let found = caller_symbols(&response);
        if response.success && expected.iter().all(|name| found.iter().any(|f| f == name)) {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "callers of target never included {expected:?}: {response:?}"
        );
        aft::runtime_drain::drain_watcher_events(ctx);
        aft::runtime_drain::drain_search_index_events(ctx);
        aft::runtime_drain::drain_callgraph_store_events(ctx);
        thread::sleep(Duration::from_millis(25));
    }
}

fn first_line(response: &Response) -> String {
    rendered(response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn borrowed_callgraph_answers_disclose_a_checkout_mismatch_and_only_then() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let (owner_root, old_commit, _) = repository(temp.path());

    let owner = configure(&owner_root, &storage);
    let owner_answer = populated_callers(&owner, &owner_root, "target");
    assert_eq!(caller_symbols(&owner_answer), vec!["newCaller".to_string()]);
    assert_unmarked(&owner_answer, "owner");

    // The owner commits while it runs. Its callgraph follows; the shared
    // search snapshot it wrote at startup does not, so a count taken against
    // that snapshot would blame every worktree at this commit for the change.
    append_caller(&owner_root, "src/target.ts", "lateHelper");
    git(&owner_root, &["commit", "-qam", "late helper"]);
    let new_commit = git(&owner_root, &["rev-parse", "HEAD"]);
    callers_including(&owner, &owner_root, &["newCaller", "lateHelper"]);

    // A worktree on the oldest commit reads the owner's graph, whose callers
    // and symbols belong to the newest commit.
    let old_worktree = linked_worktree(&owner_root, &temp.path().join("old-worktree"), &old_commit);
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
    // Between the commits target.ts changed, old_caller.ts exists only here
    // and new_caller.ts only in the graph; util.ts and the binary file are
    // the same in both. Across commits a difference is not attributed to
    // either checkout's edits, so `edits_in` stays empty.
    assert_eq!(borrowed["changed_files"], json!(3), "{:#}", answer.data);
    assert_eq!(borrowed["edits_in"], Value::Null, "{:#}", answer.data);
    assert_eq!(
        first_line(&answer),
        format!(
            "callgraph: borrowed from {} at {}; this checkout is at {} with 3 files that differ from the borrowed graph, so callers and line numbers may not match this tree",
            owner_root.display(),
            &new_commit[..7],
            &old_commit[..7],
        ),
        "rendered: {}",
        rendered(&answer)
    );

    let missing = callers(&old, &old_worktree, "legacyHelper");
    assert!(!missing.success, "legacyHelper resolved: {missing:?}");
    assert_eq!(missing.data["code"], "symbol_not_found");
    let message = missing.data["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("may exist in this checkout but not in the borrowed graph; use grep or aft_zoom on this checkout to confirm"),
        "not-found message lacks the borrowed-graph hint: {message}"
    );
    assert!(message.contains(&old_commit[..7]) && message.contains(&new_commit[..7]));
    assert!(!message.contains('\n'));

    // A worktree on the owner's commit, with no edits on either side, reads a
    // graph that describes it exactly.
    let same_worktree =
        linked_worktree(&owner_root, &temp.path().join("same-worktree"), &new_commit);
    let same = configure(&same_worktree, &storage);
    let same_answer = populated_callers(&same, &same_worktree, "target");
    let mut same_callers = caller_symbols(&same_answer);
    same_callers.sort();
    assert_eq!(same_callers, vec!["lateHelper", "newCaller"]);
    assert_unmarked(&same_answer, "same-commit worktree");
    let fits = aft::commands::callgraph_borrowed::borrowed_callgraph(&same)
        .expect("the same-commit worktree borrows the owner's graph");
    assert_eq!(fits.changed_files, Some(0), "{fits:?}");
    assert!(!fits.can_differ(), "{fits:?}");

    // Two edits in a worktree on the owner's commit are counted, and placed
    // in that worktree.
    let edited_worktree = linked_worktree(
        &owner_root,
        &temp.path().join("edited-worktree"),
        &new_commit,
    );
    append_caller(&edited_worktree, "src/util.ts", "worktreeEdit1");
    append_caller(&edited_worktree, "src/new_caller.ts", "worktreeEdit2");
    let edited = configure(&edited_worktree, &storage);
    let edited_answer = populated_callers(&edited, &edited_worktree, "target");
    let borrowed = &edited_answer.data["borrowed_callgraph"];
    assert_eq!(
        borrowed["changed_files"],
        json!(2),
        "{:#}",
        edited_answer.data
    );
    assert_eq!(borrowed["edits_in"], json!("this_checkout"));
    assert!(
        first_line(&edited_answer).contains(&format!(
            "this checkout is at {} with 2 files that differ from the borrowed graph (edits in this checkout), so callers",
            &new_commit[..7]
        )),
        "rendered: {}",
        rendered(&edited_answer)
    );

    // Three uncommitted edits in the owner reach its graph, so the clean
    // same-commit worktree now differs from it in exactly those files, and
    // the edits are placed in the borrowed checkout.
    append_caller(&owner_root, "src/target.ts", "ownerEdit1");
    append_caller(&owner_root, "src/new_caller.ts", "ownerEdit2");
    append_caller(&owner_root, "src/util.ts", "ownerEdit3");
    let owner_edits = ["ownerEdit1", "ownerEdit2", "ownerEdit3"];
    callers_including(&owner, &owner_root, &owner_edits);
    let stale = callers_including(&same, &same_worktree, &owner_edits);
    assert_eq!(stale.data["complete"], json!(false), "{:#}", stale.data);
    let borrowed = &stale.data["borrowed_callgraph"];
    assert_eq!(borrowed["changed_files"], json!(3), "{:#}", stale.data);
    assert_eq!(borrowed["edits_in"], json!("borrowed_checkout"));
    assert!(
        first_line(&stale).contains(
            "with 3 files that differ from the borrowed graph (edits in the borrowed checkout), so callers"
        ),
        "rendered: {}",
        rendered(&stale)
    );
}

fn navigate(ctx: &AppContext, op: &str, root: &Path, file: &str, symbol: &str) -> Response {
    let req = request(json!({
        "id": format!("borrowed-{op}"),
        "command": op,
        "file": root.join(file),
        "symbol": symbol,
        "depth": 1
    }));
    match op {
        "callers" => aft::commands::callers::handle_callers(&req, ctx),
        "impact" => aft::commands::impact::handle_impact(&req, ctx),
        other => panic!("unsupported op {other}"),
    }
}

/// Ask until the borrowed-graph disclosure counts `changed` differing files,
/// so the worktree's overlay has seen every file the test wrote.
fn navigate_once_differing(
    ctx: &AppContext,
    op: &str,
    root: &Path,
    file: &str,
    symbol: &str,
    changed: u64,
) -> Response {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let response = navigate(ctx, op, root, file, symbol);
        if response.data["borrowed_callgraph"]["changed_files"].as_u64() == Some(changed) {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "{op} never saw {changed} differing files: {:#}",
            response.data
        );
        aft::runtime_drain::drain_watcher_events(ctx);
        aft::runtime_drain::drain_search_index_events(ctx);
        thread::sleep(Duration::from_millis(25));
    }
}

fn second_line(op: &str, response: &Response) -> String {
    aft::subc_format::format_callgraph(op, &response.data, false)
        .lines()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

/// A caller of `newCaller`, which nothing in either commit calls.
const FRESH_CALLER: &str = "import { newCaller } from './new_caller';\n\nexport function freshCaller() {\n  return newCaller();\n}\n";

/// A configured owner at the newest commit, with its graph populated.
fn owner_with_graph(temp: &Path) -> (PathBuf, String, PathBuf, Arc<AppContext>) {
    let storage = temp.join("storage");
    let (owner_root, _, new_commit) = repository(temp);
    let owner = configure(&owner_root, &storage);
    populated_callers(&owner, &owner_root, "target");
    (owner_root, new_commit, storage, owner)
}

#[test]
fn a_caller_only_in_a_changed_file_is_found_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let (owner_root, new_commit, storage, _owner) = owner_with_graph(temp.path());
    let worktree = linked_worktree(
        &owner_root,
        &temp.path().join("fresh-worktree"),
        &new_commit,
    );
    write(&worktree, "src/fresh.ts", FRESH_CALLER);
    let ctx = configure(&worktree, &storage);

    let answer = navigate_once_differing(
        &ctx,
        "callers",
        &worktree,
        "src/new_caller.ts",
        "newCaller",
        1,
    );
    assert!(answer.success, "callers failed: {answer:?}");
    assert_eq!(answer.data["complete"], json!(false), "{:#}", answer.data);
    // The graph has no caller of newCaller; the only one lives in a file the
    // borrowed graph has never seen, and is found there by name. The import
    // line names it too but is not a caller.
    assert_eq!(
        answer.data["callers"],
        json!([{
            "file": "src/fresh.ts",
            "callers": [{
                "symbol": "freshCaller",
                "line": 4,
                "approximate": true,
                "resolved_by": "name_match"
            }]
        }]),
        "{:#}",
        answer.data
    );
    assert_eq!(answer.data["total_callers"], json!(1));
    let text = rendered(&answer);
    assert_eq!(
        second_line("callers", &answer),
        "callgraph: found 1 caller of `newCaller` by name, marked ~, in 1 file here that the borrowed graph does not reflect",
        "rendered: {text}"
    );
    assert!(
        text.contains("1 caller · 1 file group\nsrc/fresh.ts\n  ↳ freshCaller:4 ~"),
        "rendered: {text}"
    );

    let impact = navigate(&ctx, "impact", &worktree, "src/new_caller.ts", "newCaller");
    assert!(impact.success, "impact failed: {impact:?}");
    assert_eq!(impact.data["complete"], json!(false));
    assert_eq!(impact.data["total_affected"], json!(1), "{:#}", impact.data);
    assert_eq!(impact.data["affected_files"], json!(1), "{:#}", impact.data);
    let text = aft::subc_format::format_callgraph("impact", &impact.data, false);
    assert!(
        text.contains("src/fresh.ts:4\n  ↳ freshCaller ~\n  return newCaller();"),
        "rendered: {text}"
    );
    assert_eq!(
        second_line("impact", &impact),
        "callgraph: found 1 call site of `newCaller` by name, marked ~, in 1 file here that the borrowed graph does not reflect"
    );
}

#[test]
fn a_search_cut_short_by_its_file_limit_names_the_files_it_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let (owner_root, new_commit, storage, _owner) = owner_with_graph(temp.path());
    let worktree = linked_worktree(
        &owner_root,
        &temp.path().join("padded-worktree"),
        &new_commit,
    );
    // 257 files that sort before the caller's file push it past the search's
    // limit of 256 files.
    for index in 0..257 {
        write(
            &worktree,
            &format!("src/pad/p{index:03}.ts"),
            &format!("export const p{index:03} = {index};\n"),
        );
    }
    write(&worktree, "src/zz_fresh.ts", FRESH_CALLER);
    let ctx = configure(&worktree, &storage);

    let answer = navigate_once_differing(
        &ctx,
        "callers",
        &worktree,
        "src/new_caller.ts",
        "newCaller",
        258,
    );
    assert!(answer.success, "callers failed: {answer:?}");
    assert_eq!(answer.data["complete"], json!(false));
    assert_eq!(answer.data["callers"], json!([]), "{:#}", answer.data);
    let coverage = &answer.data["borrowed_coverage"];
    let text = rendered(&answer);
    // On a machine slow enough to reach the deadline first the search stops
    // earlier; either way the answer is marked partial and names the gap.
    match coverage["search_limit"].as_str() {
        Some("files") => {
            assert_eq!(coverage["files_searched"], json!(256), "{coverage:#}");
            assert_eq!(coverage["files_not_searched"], json!(2), "{coverage:#}");
            assert_eq!(
                second_line("callers", &answer),
                "callgraph: the borrowed graph does not reflect 258 files here (src/pad/p000.ts, src/pad/p001.ts, src/pad/p002.ts and 255 more); the search stopped at its limit of 256 files with 2 files not searched (src/pad/p256.ts, src/zz_fresh.ts), so grep for `newCaller` to cover the rest",
                "rendered: {text}"
            );
        }
        Some("deadline") => assert!(
            text.contains("the search stopped at its time limit")
                && text.contains("grep for `newCaller` to cover the rest"),
            "rendered: {text}"
        ),
        other => panic!("search was not cut short ({other:?}): {coverage:#}"),
    }
    assert!(text.lines().any(|line| line == "0 callers · 0 file groups"));
}

#[test]
fn a_clean_worktree_answers_exactly_as_the_owner_does() {
    let temp = tempfile::tempdir().unwrap();
    let (owner_root, new_commit, storage, owner) = owner_with_graph(temp.path());
    let worktree = linked_worktree(
        &owner_root,
        &temp.path().join("clean-worktree"),
        &new_commit,
    );
    let ctx = configure(&worktree, &storage);
    for (op, file, symbol) in [
        ("callers", "src/target.ts", "target"),
        ("callers", "src/new_caller.ts", "newCaller"),
        ("impact", "src/target.ts", "target"),
        ("impact", "src/new_caller.ts", "newCaller"),
    ] {
        let expected = navigate(&owner, op, &owner_root, file, symbol);
        let answer = navigate(&ctx, op, &worktree, file, symbol);
        assert_eq!(answer.data, expected.data, "{op} {symbol}");
        assert_eq!(
            aft::subc_format::format_callgraph(op, &answer.data, false),
            aft::subc_format::format_callgraph(op, &expected.data, false),
            "{op} {symbol}"
        );
    }
}
