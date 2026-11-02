//! `ref` on read, outline and zoom: reading a file as it is at a git
//! revision, taken from the object store without touching the working tree.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use super::helpers::{user_config_tier, AftProcess};
use crate::test_helpers::canonical_git_path;

const V1_LIB: &str = "pub fn version() -> u32 {\n    1\n}\n\npub fn only_in_v1() -> &'static str {\n    \"tagged\"\n}\n";
const MAIN_LIB: &str = "pub fn version() -> u32 {\n    2\n}\n";
const FEATURE_LIB: &str =
    "pub fn version() -> u32 {\n    3\n}\n\npub fn feature_flag() -> bool {\n    true\n}\n";
// The working-tree version. Tests look for its `    99` line (never the bare
// digits, which a commit SHA in the reply header may contain).
const WORKING_LIB: &str = "pub fn version() -> u32 {\n    99\n}\n\npub fn uncommitted() {}\n";

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    storage: PathBuf,
    outside: PathBuf,
    v1: String,
    main: String,
    feature: String,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=AFT Tests",
            "-c",
            "user.email=aft-tests@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A repository with an annotated tag `v1` (and a `release` branch at the
/// same commit), a later commit on `main`, a
/// `feature` branch with its own version, and an uncommitted working-tree
/// change, so every revision's content differs from the working file.
fn fixture() -> Fixture {
    let dir = crate::test_helpers::tempdir();
    let base = canonical_git_path(dir.path());
    let repo = base.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);

    fs::write(repo.join("src/lib.rs"), V1_LIB).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "v1"]);
    git(&repo, &["tag", "-a", "v1", "-m", "release one"]);
    // A plain branch at the same commit, so only one test depends on tag peeling.
    git(&repo, &["branch", "release"]);
    let v1 = git(&repo, &["rev-parse", "HEAD"]);

    fs::write(repo.join("src/lib.rs"), MAIN_LIB).unwrap();
    fs::write(repo.join("src/added.rs"), "pub fn added() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "main"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "--quiet", "-b", "feature"]);
    fs::write(repo.join("src/lib.rs"), FEATURE_LIB).unwrap();
    git(&repo, &["commit", "--quiet", "-am", "feature"]);
    let feature = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "--quiet", "main"]);

    fs::write(repo.join("src/lib.rs"), WORKING_LIB).unwrap();
    fs::write(repo.join("untracked.txt"), "not committed\n").unwrap();

    let outside = base.join("outside.rs");
    fs::write(&outside, "pub fn outside() {}\n").unwrap();
    let storage = base.join("storage");
    Fixture {
        _dir: dir,
        repo,
        storage,
        outside,
        v1,
        main,
        feature,
    }
}

fn configured(fixture: &Fixture, revision_reads: bool) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": fixture.repo,
            "storage_dir": fixture.storage,
            "config": [user_config_tier(json!({
                "revision_reads": revision_reads,
                "restrict_to_project_root": true,
            }))],
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");
    aft
}

fn call(aft: &mut AftProcess, name: &str, arguments: Value) -> Value {
    aft.send(
        &json!({ "id": name, "command": "tool_call", "name": name, "arguments": arguments })
            .to_string(),
    )
}

fn ok_text(resp: &Value) -> &str {
    assert_eq!(resp["success"], true, "tool call failed: {resp:?}");
    resp["text"].as_str().expect("reply text")
}

fn header(reference: &str, commit: &str) -> String {
    format!(
        "Revision `{reference}` = commit {commit} (read from git's object store, not the working tree)"
    )
}

/// Every file under the checkout (the working tree and all of `.git`) with
/// its bytes, so any write by a revision read shows up as a difference.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

#[test]
fn revision_reads_at_tag_branch_sha_and_parent_return_that_revisions_content() {
    let fixture = fixture();
    let mut aft = configured(&fixture, true);

    // An annotated tag peels to its commit.
    let tagged = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": "v1" }),
    );
    let text = ok_text(&tagged);
    assert!(text.starts_with(&header("v1", &fixture.v1)), "{text}");
    assert!(text.contains("5: pub fn only_in_v1()"), "{text}");
    assert!(
        !text.contains("    99"),
        "working-tree content leaked: {text}"
    );

    let branch = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": "feature" }),
    );
    let text = ok_text(&branch);
    assert!(
        text.starts_with(&header("feature", &fixture.feature)),
        "{text}"
    );
    assert!(text.contains("2:     3"), "{text}");
    assert!(text.contains("feature_flag"), "{text}");

    let short = &fixture.main[..10];
    let by_sha = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": short }),
    );
    let text = ok_text(&by_sha);
    assert!(text.starts_with(&header(short, &fixture.main)), "{text}");
    assert!(text.contains("2:     2"), "{text}");

    // Ranges keep their usual meaning at a revision.
    let parent = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": "HEAD~1", "startLine": 5, "endLine": 6 }),
    );
    let text = ok_text(&parent);
    assert!(text.starts_with(&header("HEAD~1", &fixture.v1)), "{text}");
    assert!(text.contains("5: pub fn only_in_v1()"), "{text}");
    assert!(!text.contains("1: pub fn version"), "{text}");
}

#[test]
fn revision_outline_and_zoom_parse_the_blob_with_the_usual_parsers() {
    let fixture = fixture();
    let mut aft = configured(&fixture, true);

    let outline = call(
        &mut aft,
        "outline",
        json!({ "target": "src/lib.rs", "ref": "release" }),
    );
    let text = ok_text(&outline);
    assert!(text.starts_with(&header("release", &fixture.v1)), "{text}");
    assert!(text.contains("only_in_v1"), "{text}");
    assert!(!text.contains("uncommitted"), "{text}");
    // The manifest spells the source with the configured project root, which
    // `fixture` already resolved to its plain (non-verbatim) canonical form.
    let source = fixture.repo.join("src").join("lib.rs");
    assert_eq!(outline["content_files"], json!([source]));
    assert_no_verbatim_paths(&outline);

    let zoom = call(
        &mut aft,
        "zoom",
        json!({ "path": "src/lib.rs", "symbols": "feature_flag", "ref": "feature" }),
    );
    let text = ok_text(&zoom);
    assert!(
        text.starts_with(&header("feature", &fixture.feature)),
        "{text}"
    );
    assert!(text.contains("pub fn feature_flag() -> bool"), "{text}");
    assert_eq!(zoom["content_files"], json!([source]));
    assert_eq!(
        zoom["targets"][0]["response"]["content_files"],
        json!([source])
    );
    assert!(!text.contains("content_files"), "{text}");
    assert_no_verbatim_paths(&zoom);

    // Every target of a batch is read at the one revision.
    let batch = call(
        &mut aft,
        "zoom",
        json!({
            "targets": [
                { "path": "src/lib.rs", "symbol": "only_in_v1" },
                { "path": "src/lib.rs", "symbol": "version" },
            ],
            "ref": "release",
        }),
    );
    let text = ok_text(&batch);
    assert!(text.starts_with(&header("release", &fixture.v1)), "{text}");
    assert!(text.contains("\"tagged\""), "{text}");
    assert!(!text.contains("    99"), "{text}");
    assert_eq!(batch["content_files"], json!([source]));
    for entry in batch["targets"].as_array().unwrap() {
        assert_eq!(entry["response"]["content_files"], json!([source]));
    }
    assert_no_verbatim_paths(&batch);
}

/// Hosts match read rules against `content_files`, and agents see these
/// paths, so none may carry the Windows verbatim (`\\?\`) prefix that
/// `fs::canonicalize` adds. Checks every string anywhere in `value`.
fn assert_no_verbatim_paths(value: &Value) {
    match value {
        Value::String(text) => assert!(!text.starts_with(r"\\?\"), "verbatim path: {text}"),
        Value::Array(items) => items.iter().for_each(assert_no_verbatim_paths),
        Value::Object(fields) => fields.values().for_each(assert_no_verbatim_paths),
        _ => {}
    }
}

#[cfg(unix)]
#[test]
fn revision_read_deny_checks_historical_location_before_today_symlink() {
    let fixture = fixture();
    fs::create_dir(fixture.repo.join("secrets")).unwrap();
    fs::create_dir(fixture.repo.join("public")).unwrap();
    let secret = fixture.repo.join("secrets/key.rs");
    let public = fixture.repo.join("public/key.rs");
    fs::write(&secret, "pub fn secret_key() {}\n").unwrap();
    fs::write(&public, "pub fn public_key() {}\n").unwrap();
    git(&fixture.repo, &["add", "secrets", "public"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "keys"]);
    fs::remove_file(&secret).unwrap();
    std::os::unix::fs::symlink(&public, &secret).unwrap();
    let mut aft = AftProcess::spawn();
    let config = aft.send(
        &json!({
            "id": "cfg", "command": "configure", "harness": "opencode",
            "project_root": fixture.repo, "storage_dir": fixture.storage,
            "config": [user_config_tier(json!({
                "revision_reads": true, "restrict_to_project_root": true,
                "sandbox": { "read_deny": [fixture.repo.join("secrets")] },
            }))],
        })
        .to_string(),
    );
    assert_eq!(config["success"], true, "{config:?}");
    for (name, mut arguments) in [
        ("read", json!({"path": "secrets/key.rs"})),
        ("outline", json!({"target": "secrets/key.rs"})),
        (
            "zoom",
            json!({"path": "secrets/key.rs", "symbols": "secret_key"}),
        ),
    ] {
        arguments["ref"] = json!("HEAD");
        let denied = call(&mut aft, name, arguments);
        assert_eq!(denied["success"], false, "{name}: {denied:?}");
        assert_eq!(denied["code"], "denied_floor", "{name}: {denied:?}");
    }
    let allowed = call(
        &mut aft,
        "read",
        json!({ "path": "public/key.rs", "ref": "HEAD" }),
    );
    assert!(ok_text(&allowed).contains("public_key"));
    let live = call(&mut aft, "read", json!({ "path": "secrets/key.rs" }));
    assert!(ok_text(&live).contains("public_key"));
}

#[test]
fn revision_read_of_a_path_missing_at_that_revision_names_the_revision() {
    let fixture = fixture();
    let mut aft = configured(&fixture, true);
    // `src/added.rs` exists in the working tree but not at `release`.
    let resp = call(
        &mut aft,
        "read",
        json!({ "path": "src/added.rs", "ref": "release" }),
    );
    assert_eq!(resp["success"], false, "{resp:?}");
    assert_eq!(resp["code"], "revision_path_not_found", "{resp:?}");
    let message = resp["message"].as_str().unwrap();
    assert!(message.contains("'release'"), "{message}");
    assert!(message.contains(&fixture.v1), "{message}");

    let unknown = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": "no-such-branch" }),
    );
    assert_eq!(unknown["code"], "revision_not_found", "{unknown:?}");
}

#[test]
fn revision_reads_off_refuses_ref_on_every_tool() {
    let fixture = fixture();
    let mut aft = configured(&fixture, false);
    for (name, arguments) in [
        ("read", json!({ "path": "src/lib.rs", "ref": "v1" })),
        ("outline", json!({ "target": "src/lib.rs", "ref": "v1" })),
        (
            "zoom",
            json!({ "path": "src/lib.rs", "symbols": "version", "ref": "v1" }),
        ),
    ] {
        let resp = call(&mut aft, name, arguments);
        assert_eq!(resp["success"], false, "{name}: {resp:?}");
        assert_eq!(resp["code"], "revision_reads_disabled", "{name}: {resp:?}");
        assert!(
            resp["message"].as_str().unwrap().contains("revision_reads"),
            "{name}: {resp:?}"
        );
    }
    // A blank placeholder is no revision and reads the working tree.
    let blank = call(&mut aft, "read", json!({ "path": "src/lib.rs", "ref": "" }));
    assert!(ok_text(&blank).contains("2:     99"));
}

#[test]
fn revision_zoom_url_target_refuses_before_repository_resolution() {
    let fixture = fixture();
    let mut aft = configured(&fixture, true);
    for batch in [
        json!([{ "target": "https://example.invalid/docs", "symbols": "Overview" }]),
        json!([
            { "target": "src/lib.rs", "symbols": "version" },
            { "target": "https://example.invalid/docs", "symbols": "Overview" },
        ]),
    ] {
        let response = call(
            &mut aft,
            "zoom",
            json!({ "batch": batch, "ref": "release" }),
        );
        assert_eq!(response["success"], false, "{response:?}");
        assert_eq!(response["code"], "invalid_request", "{response:?}");
        let message = response["message"].as_str().unwrap();
        assert!(
            message.contains("URL") && message.contains("revision"),
            "{message}"
        );
    }
}

#[test]
fn revision_read_outside_the_project_root_is_refused_like_any_read() {
    let fixture = fixture();
    let mut aft = configured(&fixture, true);
    let outside = fixture.outside.to_string_lossy().into_owned();
    for (name, arguments) in [
        ("read", json!({ "path": outside, "ref": "v1" })),
        ("outline", json!({ "target": outside, "ref": "v1" })),
        (
            "zoom",
            json!({ "path": outside, "symbols": "outside", "ref": "v1" }),
        ),
    ] {
        let resp = call(&mut aft, name, arguments);
        assert_eq!(resp["success"], false, "{name}: {resp:?}");
        assert_eq!(resp["code"], "path_outside_root", "{name}: {resp:?}");
    }
}

#[test]
fn revision_reads_leave_the_working_tree_index_and_head_unchanged() {
    let fixture = fixture();
    let mut aft = configured(&fixture, true);
    // `git status` may refresh the index, so settle it before the snapshot.
    let status_before = git(&fixture.repo, &["status", "--porcelain=v1"]);
    let before = snapshot(&fixture.repo);

    for (name, arguments) in [
        ("read", json!({ "path": "src/lib.rs", "ref": "release" })),
        ("read", json!({ "path": "src/lib.rs", "ref": "feature" })),
        ("outline", json!({ "target": "src/lib.rs", "ref": "main" })),
        (
            "zoom",
            json!({ "path": "src/lib.rs", "symbols": "version", "ref": "feature" }),
        ),
    ] {
        ok_text(&call(&mut aft, name, arguments));
    }

    let after = snapshot(&fixture.repo);
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(changed.is_empty(), "revision reads changed {changed:?}");
    assert_eq!(
        git(&fixture.repo, &["status", "--porcelain=v1"]),
        status_before
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("src/lib.rs")).unwrap(),
        WORKING_LIB
    );
    // The staged copies are private to each call and removed after it.
    let staged = fixture.storage.join("revision-reads");
    let leftovers = fs::read_dir(&staged).map(|dir| dir.count()).unwrap_or(0);
    assert_eq!(leftovers, 0, "staged blobs left in {}", staged.display());
}

#[test]
fn revision_reads_work_from_a_linked_worktree_of_the_repository() {
    let fixture = fixture();
    let linked = fixture.repo.parent().unwrap().join("linked");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--quiet",
            linked.to_str().unwrap(),
            "feature",
        ],
    );
    // Git needs an ordinary drive path to create the worktree, but the product
    // must also accept the verbatim path returned by Windows canonicalize.
    let linked = fs::canonicalize(&linked).unwrap();
    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": linked,
            "storage_dir": fixture.storage,
            "config": [user_config_tier(json!({
                "revision_reads": true,
                "restrict_to_project_root": true,
            }))],
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");

    // Refs and objects are shared with the main checkout.
    let tagged = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": "release" }),
    );
    let text = ok_text(&tagged);
    assert!(text.starts_with(&header("release", &fixture.v1)), "{text}");
    assert!(text.contains("only_in_v1"), "{text}");
    let head = call(
        &mut aft,
        "read",
        json!({ "path": "src/lib.rs", "ref": "HEAD" }),
    );
    assert!(ok_text(&head).starts_with(&header("HEAD", &fixture.feature)));
}

/// A directory that is a symlink today says nothing about the revision: the
/// revision's own `src/old/file.rs` is read, not today's link target.
#[cfg(unix)]
#[test]
fn revision_read_takes_repository_components_verbatim_through_a_parent_now_a_symlink() {
    let fixture = fixture();
    fs::create_dir_all(fixture.repo.join("src/old")).unwrap();
    fs::create_dir_all(fixture.repo.join("src/new")).unwrap();
    fs::write(
        fixture.repo.join("src/old/file.rs"),
        "pub fn old_file() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("src/new/file.rs"),
        "pub fn new_file() {}\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "src/old", "src/new"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "old and new"]);
    let split = git(&fixture.repo, &["rev-parse", "HEAD"]);
    fs::remove_dir_all(fixture.repo.join("src/old")).unwrap();
    std::os::unix::fs::symlink("new", fixture.repo.join("src/old")).unwrap();

    let mut aft = configured(&fixture, true);
    for path in [
        "src/old/file.rs".to_string(),
        fixture.repo.join("src/old/file.rs").display().to_string(),
    ] {
        let resp = call(&mut aft, "read", json!({ "path": path, "ref": split }));
        let text = ok_text(&resp);
        assert!(text.contains("old_file"), "{path}: {text}");
        assert!(!text.contains("new_file"), "{path}: {text}");
    }
}

/// The project-root check follows today's symlinks, but a revision read
/// takes the path's components verbatim. With the root restricted to
/// `public/` and `private` now a link to `public`, `private/file.txt` passes
/// the root check yet names the commit's own `private/file.txt`, outside the
/// root, so it must be refused.
#[cfg(unix)]
#[test]
fn revision_read_through_a_symlink_into_the_root_cannot_reach_a_path_outside_it() {
    let fixture = fixture();
    let public = fixture.repo.join("public");
    let private = fixture.repo.join("private");
    fs::create_dir_all(&public).unwrap();
    fs::create_dir_all(&private).unwrap();
    fs::write(public.join("file.txt"), "public words\n").unwrap();
    fs::write(private.join("file.txt"), "private words\n").unwrap();
    git(&fixture.repo, &["add", "public", "private"]);
    git(
        &fixture.repo,
        &["commit", "--quiet", "-m", "public and private"],
    );
    let split = git(&fixture.repo, &["rev-parse", "HEAD"]);
    fs::remove_dir_all(&private).unwrap();
    std::os::unix::fs::symlink("public", &private).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": public,
            "storage_dir": fixture.storage,
            "config": [user_config_tier(json!({
                "revision_reads": true,
                "restrict_to_project_root": true,
            }))],
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");

    for path in [
        private.join("file.txt").display().to_string(),
        "../private/file.txt".to_string(),
    ] {
        let resp = call(&mut aft, "read", json!({ "path": path, "ref": split }));
        assert_eq!(resp["success"], false, "{path}: {resp:?}");
        assert_eq!(resp["code"], "path_outside_root", "{path}: {resp:?}");
        assert!(
            !resp.to_string().contains("private words"),
            "{path}: {resp:?}"
        );
    }

    for path in [
        "file.txt".to_string(),
        public.join("file.txt").display().to_string(),
    ] {
        let resp = call(&mut aft, "read", json!({ "path": path, "ref": split }));
        let text = ok_text(&resp);
        assert!(text.contains("public words"), "{path}: {text}");
        assert!(text.starts_with(&header(&split, &split)), "{path}: {text}");
    }
}
