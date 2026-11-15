//! A cold build killed with SIGKILL at each stage boundary and in the
//! middle of stages, then resumed, publishes exactly what an uninterrupted
//! build publishes; inputs that changed in between are never reused stale.
//!
//! Each killed build runs in a child process: this test binary again,
//! running only [`resume_child_build`], with `AFT_TEST_KILL_AT` naming the
//! point it kills itself at (see `resume::kill_point`).

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aft_store::ids::{Digest128, FamilyId};

use super::{BuildReport, Checkout, DataError, DataLayer};

/// Files in the generated part of the project: past the resolver's batch
/// of 1,024, so passes 2-3 spool more than one range.
const GENERATED: usize = 1_100;

const CHILD: &str = "data_layer::checkout::resume_tests::resume_child_build";

fn family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"resume"))
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project with Rust modules, TypeScript calls across many files and
/// configuration files.
fn project(root: &Path) {
    write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
    write(
        root,
        "src/lib.rs",
        "mod util;\npub fn caller() { util::callee(); util::other(); }\n",
    );
    write(
        root,
        "src/util.rs",
        "pub fn callee() { other(); }\npub fn other() {}\n",
    );
    write(root, "package.json", "{\"name\": \"demo\"}\n");
    for index in 0..GENERATED {
        let previous = index.saturating_sub(1);
        write(
            root,
            &format!("gen/f{index:04}.ts"),
            &format!(
                "import {{ f{previous:04} }} from \"./f{previous:04}\";\n\
                 export class C{index} {{ run() {{ return f{previous:04}(); }} }}\n\
                 export function f{index:04}() {{ return new C{index}().run(); }}\n"
            ),
        );
    }
    write(root, "notes.txt", "nothing to see\n");
}

fn layer() -> Arc<DataLayer> {
    // A measurement on a real project sets more threads; the tests use two.
    let threads = std::env::var("AFT_TEST_RESUME_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    Arc::new(DataLayer::new(8 << 20, threads))
}

fn build(store: &Path, root: &Path) -> (Checkout, BuildReport) {
    let checkout = Checkout::bind(layer(), store, family(), root).unwrap();
    let report = checkout.build().unwrap();
    assert!(!report.superseded, "{report:?}");
    (checkout, report)
}

/// Everything the build published for the checkout: each path's content,
/// language and fragment, and every section's bytes. Segment files carry
/// ids and times of their own, so they are compared by section.
fn published(checkout: &Checkout) -> Vec<(String, Vec<u8>)> {
    let view = checkout.view().expect("a publication");
    let published = &view.published;
    assert!(published.graph_built, "the call graph was published");
    let mut out = Vec::new();
    for (index, record) in published.state.paths().iter().enumerate() {
        out.push((
            format!("path {}", String::from_utf8_lossy(&record.path)),
            format!(
                "{:?} {:?} {:?}",
                record.content,
                record.language,
                published.state.fragment(index)
            )
            .into_bytes(),
        ));
    }
    for segment in &published.segments {
        for (handle, info) in segment.sections() {
            let mut bytes = vec![0; usize::try_from(info.len).unwrap()];
            segment.read(handle, 0, &mut bytes).unwrap();
            out.push((
                format!(
                    "section {:04x}-{:04x} v{} n{}",
                    info.id.kind.get(),
                    info.id.key_space.get(),
                    info.version,
                    info.item_count
                ),
                bytes,
            ));
        }
    }
    out.sort();
    out
}

/// Asserts two publications are equal, naming the first part that differs.
fn assert_same(left: &[(String, Vec<u8>)], right: &[(String, Vec<u8>)], what: &str) {
    let names = |list: &[(String, Vec<u8>)]| {
        list.iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(left),
        names(right),
        "{what}: the published parts differ"
    );
    for ((name, a), (_, b)) in left.iter().zip(right) {
        assert!(a == b, "{what}: {name} differs");
    }
    assert!(
        left.iter().any(|(name, _)| name.starts_with("section 03")),
        "{what}: graph sections were compared"
    );
}

/// Runs the build of `root` into `store` in a child process that kills
/// itself at `point`, and checks that it died there.
fn killed_build(store: &Path, root: &Path, point: &str) {
    killed_build_of(&std::env::current_exe().unwrap(), store, root, point, &[]);
}

/// [`killed_build`] run by the test binary at `exe`, with `env` set too.
fn killed_build_of(exe: &Path, store: &Path, root: &Path, point: &str, env: &[(&str, &str)]) {
    let output = child(exe, store, root)
        .env("AFT_TEST_KILL_AT", point)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "the build was killed at {point}: {:?}\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains(&format!("KILL POINT {point}")),
        "{point}: {stderr}"
    );
}

/// The command that runs [`resume_child_build`] from the test binary at
/// `exe`, building `root` into `store`.
fn child(exe: &Path, store: &Path, root: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .args([CHILD, "--exact", "--nocapture", "--test-threads", "1"])
        .env("AFT_TEST_RESUME_CHILD", "1")
        .env("AFT_TEST_RESUME_STORE", store)
        .env("AFT_TEST_RESUME_ROOT", root)
        .env_remove("AFT_TEST_KILL_AT")
        .env_remove("AFT_TEST_HIDE_ROOT")
        .env_remove("AFT_TEST_REPLACE_EXE");
    command
}

/// Run alone by [`killed_build`] and [`no_stage_after_the_scan_reads_the_checkout`]
/// in a child process; a no-op otherwise. With `AFT_TEST_REPLACE_EXE` set,
/// the child first renames another binary over its own executable's path,
/// as an upgrade that places a new binary under a running daemon does.
#[test]
fn resume_child_build() {
    if std::env::var_os("AFT_TEST_RESUME_CHILD").is_none() {
        return;
    }
    // As a daemon does at startup, before anything can replace the binary.
    crate::pinned_executable::capture_at_startup();
    if std::env::var_os("AFT_TEST_REPLACE_EXE").is_some() {
        let exe = std::env::current_exe().unwrap();
        let mut bytes = std::fs::read(&exe).unwrap();
        bytes.extend_from_slice(format!("replaced by {}", std::process::id()).as_bytes());
        let next = exe.with_extension("next");
        std::fs::write(&next, bytes).unwrap();
        std::fs::set_permissions(&next, std::fs::metadata(&exe).unwrap().permissions()).unwrap();
        std::fs::rename(&next, &exe).unwrap();
    }
    let store = PathBuf::from(std::env::var_os("AFT_TEST_RESUME_STORE").unwrap());
    let root = PathBuf::from(std::env::var_os("AFT_TEST_RESUME_ROOT").unwrap());
    let checkout = Checkout::bind(layer(), &store, family(), &root).unwrap();
    let report = checkout.build().unwrap();
    eprintln!("CHILD BUILD FINISHED {report:?}");
}

/// The checkout's build folder in `store`, holding its lock, attempts
/// record and workspaces.
fn builds(checkout: &Checkout) -> PathBuf {
    checkout
        .family
        .dir()
        .join("builds")
        .join(checkout.checkout.to_string())
}

fn workspaces(dir: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn a_build_killed_at_any_stage_resumes_to_the_same_publication() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, fresh) = build(reference_store.path(), root.path());
    assert_eq!(fresh.resumed_records, 0);
    assert!(!fresh.resumed_passes);
    let expected = published(&reference);
    drop(reference);

    // Each point, with whether the resumed build reuses the staged records
    // and passes 2-3.
    let points = [
        ("records.append:50", false, false),
        ("records.committed", true, false),
        ("passes.range:1", true, false),
        ("passes.committed", true, true),
        ("stage.projection", true, true),
        ("stage.sites", true, true),
        ("assembly.batch:2", true, true),
        ("stage.write", true, true),
        ("stage.publish", true, true),
    ];
    let mut log = Vec::new();
    for (point, records, passes) in points {
        let store = tempfile::tempdir().unwrap();
        killed_build(store.path(), root.path(), point);
        let (checkout, report) = build(store.path(), root.path());
        assert_eq!(report.resumed_records > 0, records, "{point}: {report:?}");
        assert_eq!(report.resumed_passes, passes, "{point}: {report:?}");
        assert_same(&expected, &published(&checkout), point);
        assert_eq!(
            std::fs::read_dir(builds(&checkout))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["checkout.lock"],
            "{point}: publishing deletes the workspace and the attempts record"
        );
        log.push(format!(
            "{point}: killed, resumed with records={} passes={}, publication identical",
            report.resumed_records, report.resumed_passes
        ));
    }
    eprintln!("RESUME KILL TEST\n{}", log.join("\n"));
}

#[test]
fn an_edit_between_kill_and_resume_is_resolved_again() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "passes.committed");
    // The edit adds a call: the old passes 2-3 would miss it.
    write(
        root.path(),
        "src/util.rs",
        "pub fn callee() { other(); }\npub fn other() { callee(); }\n",
    );
    let (resumed, report) = build(store.path(), root.path());
    assert!(report.resumed_records > 0, "unchanged records are reused");
    assert!(!report.resumed_passes, "passes 2-3 are resolved again");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(
        &published(&reference),
        &published(&resumed),
        "after an edit",
    );
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+0000")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

#[test]
fn a_branch_switch_between_kill_and_resume_is_resolved_again() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    git(root.path(), &["init", "-q", "-b", "main"]);
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-q", "-m", "main"]);
    git(root.path(), &["checkout", "-q", "-b", "other"]);
    std::fs::remove_file(root.path().join("gen/f0007.ts")).unwrap();
    write(
        root.path(),
        "src/lib.rs",
        "mod util;\npub fn caller() { util::other(); }\n",
    );
    write(
        root.path(),
        "gen/extra.ts",
        "export function extra() { return 1; }\n",
    );
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-q", "-m", "other"]);
    git(root.path(), &["checkout", "-q", "main"]);

    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "stage.sites");
    git(root.path(), &["checkout", "-q", "other"]);
    let (resumed, report) = build(store.path(), root.path());
    assert!(!report.resumed_passes, "passes 2-3 are resolved again");
    assert!(report.resumed_records > 0, "unchanged records are reused");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(
        &published(&reference),
        &published(&resumed),
        "after a branch switch",
    );
}

#[test]
fn two_deaths_at_the_same_stage_start_fresh_once() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let dir = builds(&checkout);
    drop(checkout);

    killed_build(store.path(), root.path(), "stage.sites");
    let first = workspaces(&dir);
    killed_build(store.path(), root.path(), "stage.sites");
    assert_eq!(workspaces(&dir), first, "the second attempt resumed");
    // Two deaths with the same stages complete: the third attempt starts
    // from nothing in a new workspace.
    killed_build(store.path(), root.path(), "stage.sites");
    let fresh = workspaces(&dir);
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh, first, "the third attempt started fresh");
    // Only once: two more deaths there and the build still resumes.
    killed_build(store.path(), root.path(), "stage.sites");
    assert_eq!(workspaces(&dir), fresh);
    let (resumed, report) = build(store.path(), root.path());
    assert!(report.resumed_passes, "{report:?}");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(&published(&reference), &published(&resumed), "after deaths");
}

#[test]
fn a_config_file_renamed_between_kill_and_resume_is_read_under_its_new_name() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    // The same bytes are a comment in TOML and in YAML; each name reads
    // the TODO in its own grammar.
    write(root.path(), "tools/Cargo.toml", "# TODO: fix this\n");
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "records.committed");
    std::fs::rename(
        root.path().join("tools/Cargo.toml"),
        root.path().join("tools/pnpm-workspace.yaml"),
    )
    .unwrap();
    let (resumed, report) = build(store.path(), root.path());
    assert!(report.resumed_records > 0, "{report:?}");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(
        &published(&reference),
        &published(&resumed),
        "after renaming a configuration file",
    );
}

#[test]
fn a_build_never_resumes_what_another_executable_image_wrote() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let exe = bin.path().join("aft-under-test");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    // The first build runs from one image and replaces the binary at its
    // path while it runs, as an upgrade does; it is then killed.
    let replace = [("AFT_TEST_REPLACE_EXE", "1")];
    killed_build_of(&exe, store.path(), root.path(), "stage.sites", &replace);
    // The next build runs from the binary now at that path, and replaces it
    // in turn: neither image may adopt the other's work, whatever the file
    // at the path says.
    let output = child(&exe, store.path(), root.path())
        .envs(replace)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("resumed_records: 0,") && stderr.contains("resumed_passes: false"),
        "nothing was resumed: {stderr}"
    );
}

#[test]
fn a_build_killed_while_it_starts_fresh_still_starts_fresh_only_once() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let dir = builds(&checkout);
    drop(checkout);

    killed_build(store.path(), root.path(), "stage.sites");
    let first = workspaces(&dir);
    killed_build(store.path(), root.path(), "stage.sites");
    // The third attempt starts fresh and dies once its new workspace has its
    // identity, before it records the attempt as running there.
    killed_build(store.path(), root.path(), "claim.restarting");
    let fresh = workspaces(&dir);
    assert_ne!(fresh, first, "the old workspace was deleted");
    // Two more deaths there: the fresh start was used, so the build still
    // resumes.
    killed_build(store.path(), root.path(), "stage.sites");
    assert_eq!(workspaces(&dir), fresh, "the new workspace was adopted");
    killed_build(store.path(), root.path(), "stage.sites");
    let (resumed, report) = build(store.path(), root.path());
    assert!(report.resumed_passes, "no second fresh start: {report:?}");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(&published(&reference), &published(&resumed), "after deaths");
}

/// Rewrites the marker of `stage` in `workspace` with each listed file's
/// current sum, as if the files had been written as they now are.
fn bless(workspace: &Path, stage: &str) {
    let marker = super::resume::read_marker(workspace, stage)
        .unwrap()
        .unwrap();
    let mut body = Vec::new();
    for (name, _) in marker.files() {
        let sum = super::resume::sum_file(&workspace.join(name)).unwrap();
        body.extend_from_slice(&sum.len.to_le_bytes());
        body.extend_from_slice(&sum.checksum);
    }
    super::resume::write_framed(
        &super::resume::marker_path(workspace, stage),
        &marker.header,
        &body,
    )
    .unwrap();
}

/// The one workspace in the checkout's build folder `dir`.
fn only_workspace(dir: &Path) -> PathBuf {
    let names = workspaces(dir);
    assert_eq!(names.len(), 1, "{names:?}");
    dir.join(&names[0])
}

/// The files in `dir` whose names end with `suffix`.
fn files_ending(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut files = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(suffix))
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[test]
fn a_stage_that_fails_the_build_that_adopted_it_is_built_again() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "stage.sites");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let workspace = only_workspace(&builds(&checkout));
    // A staging segment damaged as it was written: its whole-file sum
    // matches, and its records fail their own checks when read.
    let segment = files_ending(&workspace.join("records"), ".seg").remove(0);
    let mut bytes = std::fs::read(&segment).unwrap();
    let middle = bytes.len() / 2;
    for byte in &mut bytes[middle..middle + 64] {
        *byte ^= 0xff;
    }
    std::fs::write(&segment, bytes).unwrap();
    bless(&workspace, super::resume::RECORDS);
    assert!(
        checkout.build().is_err(),
        "the damaged records fail the build"
    );
    let report = checkout.build().unwrap();
    assert_eq!(report.resumed_records, 0, "{report:?}");
    assert!(!report.resumed_passes, "{report:?}");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(
        &published(&reference),
        &published(&checkout),
        "after a stage failed its build",
    );
}

#[test]
fn a_passes_stage_that_cannot_be_adopted_is_not_reported_resumed() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "stage.sites");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let workspace = only_workspace(&builds(&checkout));
    // A range that names a file its batch does not hold, with every sum
    // matching: the stage passes its checks, and adopting it fails.
    let range = files_ending(&workspace.join("runs"), "")
        .into_iter()
        .find(|path| path.to_string_lossy().contains("resolutions-"))
        .unwrap();
    let (mut header, body) = super::resume::read_framed(&range).unwrap();
    header.members[0] = b"gen/missing.ts".to_vec();
    super::resume::write_framed(&range, &header, &body).unwrap();
    bless(&workspace, super::resume::PASSES);
    let report = checkout.build().unwrap();
    assert!(report.resumed_records > 0, "{report:?}");
    assert!(!report.resumed_passes, "{report:?}");
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(
        &published(&reference),
        &published(&checkout),
        "after a stage could not be adopted",
    );
}

#[test]
fn a_build_from_another_executable_or_a_damaged_stage_is_not_reused() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "stage.sites");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let dir = builds(&checkout);
    let workspace = dir.join(&workspaces(&dir)[0]);
    // A damaged spooled range drops passes 2-3 but keeps the records.
    let runs = workspace.join("runs");
    let range = std::fs::read_dir(&runs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("resolutions-"))
        .unwrap();
    let mut bytes = std::fs::read(&range).unwrap();
    bytes[10] ^= 1;
    std::fs::write(&range, bytes).unwrap();
    let report = checkout.build().unwrap();
    assert!(report.resumed_records > 0, "{report:?}");
    assert!(!report.resumed_passes, "{report:?}");
    drop(checkout);

    // A workspace whose identity names another build is never adopted.
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "stage.sites");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let dir = builds(&checkout);
    let workspace = dir.join(&workspaces(&dir)[0]);
    let mut identity = aft_store::family::workspace::read_identity(&workspace).unwrap();
    identity.payload = String::from_utf8(identity.payload)
        .unwrap()
        .replace("executable=", "executable=another-")
        .into_bytes();
    aft_store::family::workspace::write_identity(&workspace, &identity).unwrap();
    let report = checkout.build().unwrap();
    assert_eq!(report.resumed_records, 0, "{report:?}");
    assert!(!report.resumed_passes, "{report:?}");
    assert!(!workspace.exists(), "the foreign workspace was deleted");
}

#[test]
fn no_stage_after_the_scan_reads_the_checkout() {
    let root = tempfile::tempdir().unwrap();
    let checkout_root = root.path().join("checkout");
    project(&checkout_root);
    let store = tempfile::tempdir().unwrap();
    // The child moves the checkout away once it is scanned and back before
    // the segment is written: every stage between reads only what the scan
    // captured.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([CHILD, "--exact", "--nocapture", "--test-threads", "1"])
        .env("AFT_TEST_RESUME_CHILD", "1")
        .env("AFT_TEST_RESUME_STORE", store.path())
        .env("AFT_TEST_RESUME_ROOT", &checkout_root)
        .env("AFT_TEST_HIDE_ROOT", &checkout_root)
        .env_remove("AFT_TEST_KILL_AT")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("HIDE ROOT at stage.resolve"), "{stderr}");
    assert!(stderr.contains("HIDE ROOT at stage.write"), "{stderr}");
    assert!(stderr.contains("CHILD BUILD FINISHED"), "{stderr}");
    let hidden = Checkout::bind(layer(), store.path(), family(), &checkout_root).unwrap();
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), &checkout_root);
    assert_same(
        &published(&reference),
        &published(&hidden),
        "with the checkout hidden after the scan",
    );
}

#[test]
fn below_the_floor_a_build_first_deletes_dead_workspaces() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), root.path(), "stage.sites");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let dir = builds(&checkout);
    assert_eq!(workspaces(&dir).len(), 1, "the killed build left its work");
    let _low = crate::disk_floor::set_free_space_for_test(
        store.path(),
        crate::disk_floor::MIN_FREE_BYTES - 1,
        10 * 1024 * 1024 * 1024,
    );
    let error = checkout.build().unwrap_err();
    assert!(matches!(error, DataError::DiskFloor(_)), "{error}");
    assert!(
        workspaces(&dir).is_empty(),
        "the dead workspace was deleted to make room: {:?}",
        workspaces(&dir)
    );
}

#[test]
fn a_refused_admission_writes_no_workspace() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    checkout.build().unwrap();
    let dir = builds(&checkout);
    // Exactly at the floor: the build starts, and the first write into a
    // new workspace (the file naming which build it belongs to) is refused.
    let _low = crate::disk_floor::set_free_space_for_test(
        store.path(),
        crate::disk_floor::MIN_FREE_BYTES,
        10 * 1024 * 1024 * 1024,
    );
    std::fs::write(root.path().join("src/util.rs"), "pub fn callee() {}\n").unwrap();
    let error = checkout.build().unwrap_err();
    assert!(matches!(error, DataError::DiskFloor(_)), "{error}");
    assert_eq!(
        std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["checkout.lock"],
        "nothing was written"
    );
}

/// Times a cold build of the checkout `AFT_TEST_RESUME_PROJECT` names, then
/// for each of several kill points a build killed there and the build that
/// resumes it, and checks each resumed publication equals the cold one.
#[test]
#[ignore = "measurement; AFT_TEST_RESUME_PROJECT names a checkout to build"]
fn measure_resume_on_a_project() {
    let root = PathBuf::from(
        std::env::var_os("AFT_TEST_RESUME_PROJECT").expect("AFT_TEST_RESUME_PROJECT"),
    );
    let reference_store = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (reference, fresh) = build(reference_store.path(), &root);
    let fresh_ms = started.elapsed().as_millis();
    let expected = published(&reference);
    drop(reference);
    eprintln!(
        "RESUME MEASURE cold files={} parsed={} wall_ms={fresh_ms} scan_ms={:.0} resolve_ms={:.0} write_ms={:.0}",
        fresh.files, fresh.parsed_files, fresh.scan_ms, fresh.resolve_ms, fresh.write_ms
    );
    for point in ["records.committed", "passes.committed", "stage.sites"] {
        let store = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        killed_build(store.path(), &root, point);
        let killed_ms = started.elapsed().as_millis();
        let started = std::time::Instant::now();
        let (checkout, report) = build(store.path(), &root);
        let resumed_ms = started.elapsed().as_millis();
        assert_same(&expected, &published(&checkout), point);
        eprintln!(
            "RESUME MEASURE point={point} killed_after_ms={killed_ms} resumed_wall_ms={resumed_ms} scan_ms={:.0} resolve_ms={:.0} write_ms={:.0} resumed_records={} resumed_passes={} identical=yes",
            report.scan_ms,
            report.resolve_ms,
            report.write_ms,
            report.resumed_records,
            report.resumed_passes
        );
    }
}
