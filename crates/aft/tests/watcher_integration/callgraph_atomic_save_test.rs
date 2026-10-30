//! An editor's atomic save (write a sibling temporary file, then rename it
//! over the original) must reach the call graph the same way an in-place write
//! does. While the graph has not taken the edit, an answer must say so with
//! `complete: false` instead of serving the old edge as a complete answer.
//!
//! Both checkout kinds run against a live OS watcher: a plain checkout with
//! views off (legacy callgraph store) and a linked worktree with views on
//! (per-checkout view publication).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::helpers::AftProcess;
use crate::test_helpers::canonical_git_path;

const ALPHA_SOURCE: &str = "export function probe_alpha(): number { return 1; }\n\
export function probe_beta(): number { return 2; }\n\
export function entry(): number { return probe_alpha(); }\n";

const BETA_SOURCE: &str = "export function probe_alpha(): number { return 1; }\n\
export function probe_beta(): number { return 2; }\n\
export function entry(): number { return probe_beta(); } // ATOMIC_SAVE_MARKER\n";

/// How long the edited edge may take to appear. A small project refreshes in
/// well under a second; the rest is headroom for a loaded test machine.
const REFRESH_DEADLINE: Duration = Duration::from_secs(30);

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_all(root: &Path) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Atomic Save Test",
            "-c",
            "user.email=atomic-save@example.test",
            "commit",
            "-qm",
            "probe",
        ],
    );
}

fn init_repo(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("src/probe.ts"), ALPHA_SOURCE).unwrap();
    commit_all(root);
}

/// Save `content` the way most editors do: write a sibling temporary file and
/// rename it over the original, so the path gets a new inode and the watcher
/// reports a rename rather than a content write.
fn atomic_save(file: &Path, content: &str) {
    let temp = file.with_file_name(format!(
        ".{}.aft-atomic-save.tmp",
        file.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&temp, content).unwrap();
    std::fs::rename(&temp, file).unwrap();
}

fn configure(aft: &mut AftProcess, root: &Path, storage: &Path, views: bool) {
    let response = aft.send(
        &json!({
            "id": "configure-atomic-save",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": crate::helpers::user_config(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": true },
                "views": { "enabled": views },
                "worktree": { "ram_overlay": false },
            })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:#}");
}

fn call_tree(aft: &mut AftProcess, file: &Path) -> Value {
    aft.send(
        &json!({
            "id": "atomic-save-call-tree",
            "command": "call_tree",
            "file": file,
            "symbol": "entry",
            "depth": 1,
        })
        .to_string(),
    )
}

fn callers(aft: &mut AftProcess, file: &Path, symbol: &str) -> Value {
    aft.send(
        &json!({
            "id": "atomic-save-callers",
            "command": "callers",
            "file": file,
            "symbol": symbol,
            "depth": 1,
        })
        .to_string(),
    )
}

/// True once the trigram index answers with the edit. The index learns of a
/// file change only from the watcher, so an indexed (not filesystem-walk)
/// match proves the daemon has received the save's watcher events.
fn index_sees_edit(aft: &mut AftProcess) -> bool {
    let response = aft.send(
        &json!({
            "id": "atomic-save-grep",
            "command": "grep",
            "pattern": "ATOMIC_SAVE_MARKER",
        })
        .to_string(),
    );
    response["matches"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
        && response["fallback"].is_null()
        && response["index"]["trigram"]["status"] == "ready"
}

fn child_names(response: &Value) -> Vec<String> {
    response["children"]
        .as_array()
        .map(|children| {
            children
                .iter()
                .filter_map(|child| child["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn caller_symbols(response: &Value) -> Vec<String> {
    response["callers"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["callers"].as_array().cloned().unwrap_or_default())
        .filter_map(|caller| caller["symbol"].as_str().map(str::to_string))
        .collect()
}

/// The graph answer can only be honest about a pending edit once the daemon
/// has seen the watcher event. Once the trigram index reflects the edit the
/// event has arrived: from then on, a call-graph answer that still shows the
/// old edge must be marked incomplete.
fn assert_stale_answer_is_marked(response: &Value, what: &str) {
    assert_eq!(
        response["complete"], false,
        "{what} still shows the pre-save edge after the watcher delivered the save, \
         but does not say the answer is incomplete: {response:#}"
    );
    // Once the watcher drain has sorted the event, the gap names the file;
    // before that, only the generic watcher gap can be present.
    let gaps = response["gaps"].as_array().cloned().unwrap_or_default();
    assert!(
        gaps.iter().any(|gap| {
            gap["kind"] == "watcher_pending"
                || gap["path"]
                    .as_str()
                    .is_some_and(|path| path.replace('\\', "/") == "src/probe.ts")
        }),
        "{what} is marked incomplete but names neither the saved file nor pending \
         watcher work: {response:#}"
    );
}

/// Wait for the graph to be served for the original file, so the test starts
/// from a known edge.
fn wait_for_initial_edge(aft: &mut AftProcess, file: &Path) {
    let deadline = Instant::now() + REFRESH_DEADLINE;
    loop {
        let response = call_tree(aft, file);
        if response["success"] == true && child_names(&response) == ["probe_alpha"] {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the initial call graph never served entry -> probe_alpha: {response:#}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the test has learned about whether the daemon has received the save's
/// watcher events, and how many stale answers said they were incomplete.
#[derive(Default)]
struct SaveEvidence {
    seen: bool,
    stale_marked: usize,
}

impl SaveEvidence {
    /// Check one answer against what was known when it was requested, then
    /// learn from it. Call this before the next question is sent: evidence
    /// that arrives later (the watcher can deliver the save between two
    /// questions) says nothing about what the daemon knew for this answer.
    fn judge(&mut self, response: &Value, fresh: bool, what: &str) {
        if response["success"] != true || fresh {
            return;
        }
        if self.seen || response["complete"] == false {
            assert_stale_answer_is_marked(response, what);
            self.stale_marked += 1;
        }
        self.seen = self.seen || response["complete"] == false;
    }
}

/// One poll: the answers it got and whether both show the edited edge.
struct PollRound {
    tree: Value,
    callers: Value,
    fresh: bool,
}

/// Ask `call_tree` and then `callers` once, running `between` after the first
/// question and before the second. Each answer is checked against what
/// `evidence` held when that answer was requested.
fn poll_round(
    aft: &mut AftProcess,
    file: &Path,
    evidence: &mut SaveEvidence,
    between: impl FnOnce(&mut AftProcess),
) -> PollRound {
    aft.send(r#"{"id":"atomic-save-ping","command":"ping"}"#);
    evidence.seen = evidence.seen || index_sees_edit(aft);
    let tree = call_tree(aft, file);
    let tree_fresh = tree["success"] == true && child_names(&tree) == ["probe_beta"];
    evidence.judge(&tree, tree_fresh, "call_tree");
    between(aft);
    let beta_callers = callers(aft, file, "probe_beta");
    let callers_fresh =
        beta_callers["success"] == true && caller_symbols(&beta_callers) == ["entry"];
    evidence.judge(&beta_callers, callers_fresh, "callers");
    PollRound {
        tree,
        callers: beta_callers,
        fresh: tree_fresh && callers_fresh,
    }
}

/// Poll until `call_tree` and `callers` both reflect the edited edge, starting
/// from what `evidence` already holds.
fn poll_until_fresh(
    aft: &mut AftProcess,
    file: &Path,
    saved: Instant,
    evidence: &mut SaveEvidence,
) {
    let deadline = saved + REFRESH_DEADLINE;
    loop {
        let round = poll_round(aft, file, evidence, |_| {});
        if round.fresh {
            eprintln!(
                "atomic save reached the call graph after {} ms; {} stale answers were \
                 marked incomplete on the way",
                saved.elapsed().as_millis(),
                evidence.stale_marked
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the call graph never took the atomic save within {REFRESH_DEADLINE:?} \
             (daemon saw it: {})\ncall_tree: {:#}\ncallers: {:#}",
            evidence.seen,
            round.tree,
            round.callers
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait for the initial edge, then give the freshly bound watcher a moment to
/// settle, as editors save well after a project is opened.
fn prepare_for_save(aft: &mut AftProcess, file: &Path) {
    wait_for_initial_edge(aft, file);
    std::thread::sleep(Duration::from_millis(500));
}

/// Atomic-save `file` and poll until `call_tree` and `callers` both reflect
/// the edited edge. Once the daemon has the save's watcher event (the trigram
/// index answers with the edit, or an earlier answer was already marked
/// pending), every answer that still shows the old edge must carry
/// `complete: false`: the daemon cannot take back knowledge of a pending edit.
fn assert_atomic_save_reaches_callgraph(aft: &mut AftProcess, file: &Path) {
    prepare_for_save(aft, file);
    atomic_save(file, BETA_SOURCE);
    let saved = Instant::now();
    poll_until_fresh(aft, file, saved, &mut SaveEvidence::default());
}

#[test]
fn atomic_save_reaches_callgraph_on_a_plain_checkout_with_views_off() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("project");
    init_repo(&root);
    let mut aft = AftProcess::spawn_with_real_watcher();
    configure(&mut aft, &root, &base.join("storage"), false);
    assert_atomic_save_reaches_callgraph(&mut aft, &root.join("src/probe.ts"));
    assert!(aft.shutdown().success());
}

/// The watcher can deliver a save between two questions of one poll. The
/// first answer was given before the daemon knew about the edit, so serving
/// the old edge as complete was honest; only answers requested after the
/// daemon received the save must say they are incomplete. The watcher intake
/// seam places the delivery exactly there instead of leaving it to timing.
#[cfg(feature = "test-timing-hooks")]
#[test]
fn an_answer_given_before_the_watcher_took_the_save_is_not_judged_by_later_evidence() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("project");
    init_repo(&root);
    let gate = base.join("watcher-intake.gate");
    let taken = base.join("watcher-intake.gate.taken");
    let mut aft = AftProcess::spawn_with_real_watcher_env(&[(
        "AFT_TEST_WATCHER_INTAKE_GATE_FILE",
        gate.as_os_str(),
    )]);
    configure(&mut aft, &root, &base.join("storage"), false);
    let file = root.join("src/probe.ts");
    prepare_for_save(&mut aft, &file);

    // Hold the watcher's intake across the save and the first question.
    std::fs::write(&gate, b"held").unwrap();
    // Hold its flush until both answers are in, so the daemon cannot refresh
    // the call graph before `callers` is asked however long this test pauses.
    let flush_hold = base.join("watcher-intake.gate.flush-hold");
    std::fs::write(&flush_hold, b"held").unwrap();
    atomic_save(&file, BETA_SOURCE);
    let saved = Instant::now();
    let mut evidence = SaveEvidence::default();
    let round = poll_round(&mut aft, &file, &mut evidence, |_| {
        // Release the save to the daemon and wait until its watcher thread
        // has taken the source file in, before the second question.
        std::fs::remove_file(&gate).unwrap();
        let wanted = file.display().to_string();
        let deadline = Instant::now() + REFRESH_DEADLINE;
        while !std::fs::read_to_string(&taken)
            .unwrap_or_default()
            .lines()
            .any(|line| line == wanted)
        {
            assert!(
                Instant::now() < deadline,
                "the watcher never took in the released save"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    std::fs::remove_file(&flush_hold).unwrap();
    // The round must really have straddled the delivery, or this test proves
    // nothing: the first answer predates it, the second knows about it.
    assert!(
        round.tree["success"] == true
            && child_names(&round.tree) == ["probe_alpha"]
            && round.tree["complete"].is_null(),
        "call_tree was asked while the watcher held the save, so it serves the old edge \
         without knowing it is stale: {:#}",
        round.tree
    );
    assert!(
        round.callers["complete"] == false
            && round.callers["gaps"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|gap| gap["path"] == "src/probe.ts"),
        "callers was asked after the watcher took the save, so it names the pending file: {:#}",
        round.callers
    );
    poll_until_fresh(&mut aft, &file, saved, &mut evidence);
    assert!(aft.shutdown().success());
}

#[test]
fn atomic_save_reaches_callgraph_on_a_linked_worktree_with_views_on() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let temp = tempfile::tempdir().unwrap();
    let base = canonical_git_path(temp.path());
    let owner = base.join("owner");
    init_repo(&owner);
    let checkout: PathBuf = base.join("checkout");
    git(
        &owner,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            checkout.to_str().unwrap(),
        ],
    );
    let mut aft = AftProcess::spawn_with_real_watcher();
    configure(&mut aft, &checkout, &base.join("storage"), true);
    // With views on, every call-graph answer comes from a bind of the
    // checkout's v3 view that verifies the files against the disk, so there is
    // no refresh window to observe. A stale answer, should one be served, must
    // still say it is incomplete.
    assert_atomic_save_reaches_callgraph(&mut aft, &checkout.join("src/probe.ts"));
    assert!(aft.shutdown().success());
}
