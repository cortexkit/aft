//! Characterization of how separate `aft` processes share one legacy
//! (views-off) call-graph store on the same project root and storage folder.
//!
//! A cold build writes a brand-new generation file named
//! `<key>.g<nanos>.<pid>.sqlite` and points `<key>.current` at it, so the
//! pointer's contents identify the build that produced the store being served.
//! If a later process serves the same pointer, nothing rebuilt the store in
//! between; a different pointer names a newer published generation, that is,
//! a cold build ran (in whichever process published it).
//!
//! Each process is a real child `aft` with its own HOME and XDG folders; only
//! the explicit `storage_dir` is shared, which is how several editor sessions
//! on one machine share a store.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::helpers::AftProcess;

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

/// A small git repository whose `target` function has one caller per module.
fn project(parent: &Path, modules: usize) -> PathBuf {
    project_named(parent, "project", modules)
}

fn project_named(parent: &Path, name: &str, modules: usize) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("target.ts"),
        "export function target(value: number) { return value; }\n",
    )
    .unwrap();
    for index in 0..modules {
        std::fs::write(
            root.join(format!("caller{index}.ts")),
            format!(
                "import {{ target }} from './target';\n\
                 export function caller{index}(value: number) {{ return target(value); }}\n"
            ),
        )
        .unwrap();
    }
    git(&root, &["init", "-q"]);
    commit(&root);
    std::fs::canonicalize(root).unwrap()
}

fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Callgraph Test",
            "-c",
            "user.email=callgraph@example.test",
            "commit",
            "-qm",
            "callers",
        ],
    );
}

/// An independent clone of `source` (not a linked worktree) with one more
/// caller committed. Both checkouts share the repository's root commit.
fn diverged_clone(source: &Path, parent: &Path) -> PathBuf {
    let clone = parent.join("clone");
    git(
        parent,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    std::fs::write(
        clone.join("cloneonly.ts"),
        "import { target } from './target';\n\
         export function cloneOnly(value: number) { return target(value); }\n",
    )
    .unwrap();
    commit(&clone);
    std::fs::canonicalize(clone).unwrap()
}

fn configure(aft: &mut AftProcess, root: &Path, storage: &Path) {
    let request = json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root,
        "storage_dir": storage,
        "config": crate::test_helpers::user_config(json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": true },
            "views": { "enabled": false }
        }))
    });
    let response = aft.send(&request.to_string());
    assert_eq!(response["success"], true, "configure: {response:?}");
}

/// Ask for `target`'s callers until the store answers. The test harness lets
/// the first call-graph request wait for a build to finish; a request that
/// still answers `callgraph_building` is retried.
fn callers(aft: &mut AftProcess, root: &Path) -> Value {
    let request = json!({
        "id": "callers",
        "command": "callers",
        "file": root.join("target.ts"),
        "symbol": "target"
    })
    .to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let response = aft.send(&request);
        if response["code"] != "callgraph_building" {
            assert_eq!(response["success"], true, "callers: {response:?}");
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "callgraph never became ready: {response:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn callers_once(aft: &mut AftProcess, root: &Path) -> Value {
    aft.send(
        &json!({
            "id": "immediate-callers", "command": "callers",
            "file": root.join("target.ts"), "symbol": "target"
        })
        .to_string(),
    )
}

fn callers_after_comparison(aft: &mut AftProcess, root: &Path) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let response = callers(aft, root);
        if response["complete"] != false {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "comparison never settled: {response:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The contents of the single `<key>.current` pointer under
/// `storage/callgraph`. Other artifact families keep their own pointers.
fn published_generation(storage: &Path) -> String {
    let mut files = Vec::new();
    collect(&storage.join("callgraph"), &mut files);
    let pointers: Vec<_> = files
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "current"))
        .collect();
    assert_eq!(
        pointers.len(),
        1,
        "expected one call-graph pointer, found {pointers:?}"
    );
    std::fs::read_to_string(pointers[0])
        .unwrap()
        .trim()
        .to_string()
}

#[test]
#[ignore = "fails today (#746): independent clones share one store key, so a clone is served the other clone's graph"]
fn legacy_callgraph_two_clones_of_one_repository_keep_their_stores_across_restarts() {
    let fixture = tempfile::tempdir().unwrap();
    let first_root = project_named(fixture.path(), "first", 3);
    let clone_root = diverged_clone(&first_root, fixture.path());
    let storage = fixture.path().join("storage");

    let mut first = AftProcess::spawn();
    configure(&mut first, &first_root, &storage);
    assert_eq!(callers(&mut first, &first_root)["total_callers"], 3);
    let first_built = published_generation(&storage);
    first.shutdown();

    // The clone opens next, on its own and with the first checkout closed.
    let mut clone = AftProcess::spawn();
    configure(&mut clone, &clone_root, &storage);
    let clone_answer = callers(&mut clone, &clone_root);
    let after_clone = published_generation(&storage);
    let (_, clone_log) = clone.stderr_output();
    assert_eq!(
        clone_answer["total_callers"], 4,
        "the clone must see its own extra caller: {clone_answer:?}\nclone log:\n{clone_log}"
    );

    // The first checkout restarts with nothing changed on its disk.
    let mut restarted = AftProcess::spawn();
    configure(&mut restarted, &first_root, &storage);
    let restart_answer = callers(&mut restarted, &first_root);
    let after_restart = published_generation(&storage);
    let (_, restart_log) = restarted.stderr_output();
    assert_eq!(
        restart_answer["total_callers"], 3,
        "the first checkout must not see the clone's caller: {restart_answer:?}"
    );
    assert_eq!(
        after_restart, first_built,
        "an unchanged checkout that restarts must serve the store it built, \
         not cold-build because a clone of the same repository built in \
         between (clone published {after_clone}); restart log:\n{restart_log}"
    );
}

#[test]
#[ignore = "fails today (#746): a clone opened while another clone runs reads that clone's graph"]
fn legacy_callgraph_clone_opened_while_another_clone_runs_sees_its_own_files() {
    let fixture = tempfile::tempdir().unwrap();
    let first_root = project_named(fixture.path(), "first", 3);
    let clone_root = diverged_clone(&first_root, fixture.path());
    let storage = fixture.path().join("storage");

    let mut first = AftProcess::spawn();
    configure(&mut first, &first_root, &storage);
    assert_eq!(callers(&mut first, &first_root)["total_callers"], 3);

    let mut clone = AftProcess::spawn();
    configure(&mut clone, &clone_root, &storage);
    let clone_answer = callers(&mut clone, &clone_root);
    let (_, clone_log) = clone.stderr_output();
    first.shutdown();
    assert_eq!(
        clone_answer["total_callers"], 4,
        "a clone opened while another clone of the same repository runs must \
         answer from its own files: {clone_answer:?}\nclone log:\n{clone_log}"
    );
}
