//! End-to-end sequences for the shared search artifact that a repository's
//! home checkout publishes and its linked worktrees borrow.
//!
//! Each "session" is a fresh `aft` process pointed at one directory, all
//! sharing one isolated storage directory and a throwaway HOME, which is how
//! separate editor sessions meet on a real machine.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{canonicalize_like_product, user_config, AftProcess};

const TOKENS: [&str; 3] = ["TOKEN_TOP", "TOKEN_ONE", "TOKEN_TWO"];

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    worktree: PathBuf,
    storage: PathBuf,
    user_home: PathBuf,
}

#[derive(Debug)]
struct SessionOutcome {
    /// Match counts for `TOKEN_TOP`, `TOKEN_ONE`, `TOKEN_TWO`, in that order.
    counts: [u64; 3],
    index_status: String,
    /// `outcome=` values of every borrowed search `artifact_loaded` event.
    borrowed_outcomes: Vec<String>,
    /// Number of search `build_ready` events, i.e. owner rebuilds.
    builds: usize,
}

fn git(dir: &Path, args: &[&str]) {
    let status = crate::test_helpers::apply_hermetic_git_env(Command::new("git").current_dir(dir))
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("fixture dir");
    // Git refuses Windows verbatim (`\\?\`) paths, which bare
    // `fs::canonicalize` returns there, so use the product's normalized form.
    let base = canonicalize_like_product(dir.path());
    let home = base.join("home");
    for (path, token) in [
        ("top.ts", "TOKEN_TOP"),
        ("sub1/one.ts", "TOKEN_ONE"),
        ("sub2/two.ts", "TOKEN_TWO"),
    ] {
        let file = home.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, format!("export const value = \"{token}\";\n")).unwrap();
    }
    fs::write(home.join(".gitignore"), "node_modules/\n").unwrap();
    git(&home, &["init", "-q"]);
    git(&home, &["config", "user.email", "scope@example.com"]);
    git(&home, &["config", "user.name", "Scope Test"]);
    git(&home, &["add", "."]);
    git(&home, &["commit", "-q", "-m", "fixture"]);
    fs::write(home.join(".git/info/exclude"), "*.local\n").unwrap();
    let worktree = base.join("wt");
    git(
        &home,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    let storage = base.join("storage");
    let user_home = base.join("user-home");
    fs::create_dir_all(&storage).unwrap();
    fs::create_dir_all(&user_home).unwrap();
    Fixture {
        _dir: dir,
        home,
        worktree,
        storage,
        user_home,
    }
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn run_session(fixture: &Fixture, root: &Path) -> SessionOutcome {
    let xdg = fixture.user_home.join("xdg");
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", fixture.storage.as_os_str()),
        ("HOME", fixture.user_home.as_os_str()),
        ("XDG_CONFIG_HOME", xdg.join("config").as_os_str()),
        ("XDG_DATA_HOME", xdg.join("data").as_os_str()),
        ("XDG_CACHE_HOME", xdg.join("cache").as_os_str()),
    ]);
    let configured = send(
        &mut aft,
        json!({
            "id": "scope-configure",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "storage_dir": fixture.storage.display().to_string(),
            "config": user_config(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false,
                "worktree": { "ram_overlay": true }
            })),
        }),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    // A borrowed load that is refused never becomes ready, so this wait is
    // bounded and the grep below reports whatever state the session reached.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let status = send(
            &mut aft,
            json!({ "id": "scope-status", "command": "status" }),
        );
        if status["search_index"]["status"] == "ready" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let mut counts = [0u64; 3];
    let mut index_status = String::new();
    for (slot, token) in TOKENS.iter().enumerate() {
        let response = send(
            &mut aft,
            json!({
                "id": format!("scope-grep-{token}"),
                "command": "grep",
                "pattern": token,
                "path": ".",
                "limit": 20
            }),
        );
        counts[slot] = response["total_matches"].as_u64().unwrap_or(0);
        index_status = response["index_status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
    }

    let (status, stderr) = aft.stderr_output();
    assert!(status.success(), "aft exited badly: {stderr}");
    let search_events = stderr
        .lines()
        .filter(|line| line.contains("index_event ") && line.contains("plane=search"));
    let mut borrowed_outcomes = Vec::new();
    let mut builds = 0;
    for line in search_events {
        if line.contains("kind=artifact_loaded") && line.contains("borrowed=true") {
            if let Some(outcome) = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("outcome="))
            {
                borrowed_outcomes.push(outcome.to_string());
            }
        }
        if line.contains("kind=build_ready") {
            builds += 1;
        }
    }
    SessionOutcome {
        counts,
        index_status,
        borrowed_outcomes,
        builds,
    }
}

/// A worktree whose ignore files match the home checkout's must borrow the
/// home checkout's artifact as fully current. `.git/info/exclude` lives in the
/// shared git directory: inside the home checkout, outside every worktree.
#[test]
fn worktree_borrow_of_home_artifact_is_ready_when_ignore_rules_match() {
    let fixture = fixture();
    let home = run_session(&fixture, &fixture.home);
    eprintln!("home: {home:?}");
    assert_eq!(home.counts, [1, 1, 1]);

    let worktree = run_session(&fixture, &fixture.worktree);
    eprintln!("worktree: {worktree:?}");
    assert_eq!(worktree.counts, [1, 1, 1]);
    assert_eq!(worktree.index_status, "Ready");
    assert_eq!(worktree.borrowed_outcomes, vec!["ready".to_string()]);
}

/// The sequence from the subfolder report: a session opened in a subfolder
/// must neither narrow the artifact that the top level and worktrees borrow,
/// nor keep overwriting a sibling subfolder's artifact.
#[test]
fn subfolder_session_never_narrows_the_repository_artifact() {
    let fixture = fixture();
    let sub1 = fixture.home.join("sub1");
    let sub2 = fixture.home.join("sub2");

    let step1 = run_session(&fixture, &fixture.home);
    eprintln!("1 home: {step1:?}");
    assert_eq!(step1.counts, [1, 1, 1]);

    let step2 = run_session(&fixture, &fixture.worktree);
    eprintln!("2 worktree: {step2:?}");
    assert_eq!(step2.counts, [1, 1, 1]);

    let step3 = run_session(&fixture, &sub1);
    eprintln!("3 home/sub1: {step3:?}");
    assert_eq!(step3.counts, [0, 1, 0]);

    let step4 = run_session(&fixture, &fixture.worktree);
    eprintln!("4 worktree: {step4:?}");
    assert_eq!(
        step4.counts,
        [1, 1, 1],
        "a subfolder session narrowed the artifact the worktree borrows"
    );
    assert_eq!(step4.index_status, "Ready");
    assert_eq!(step4.borrowed_outcomes, vec!["ready".to_string()]);

    let step5 = run_session(&fixture, &sub2);
    eprintln!("5 home/sub2: {step5:?}");
    assert_eq!(step5.counts, [0, 0, 1]);

    // Each subfolder owns its own artifact, so returning to sub1 reuses the
    // artifact it wrote in step 3 instead of rebuilding over sub2's.
    let step6 = run_session(&fixture, &sub1);
    eprintln!("6 home/sub1 again: {step6:?}");
    assert_eq!(step6.counts, [0, 1, 0]);
    assert_eq!(step6.builds, 0, "sibling subfolders overwrote each other");

    let step7 = run_session(&fixture, &fixture.home);
    eprintln!("7 home again: {step7:?}");
    assert_eq!(step7.counts, [1, 1, 1]);
    assert_eq!(step7.builds, 0, "the repository artifact was disturbed");
}
