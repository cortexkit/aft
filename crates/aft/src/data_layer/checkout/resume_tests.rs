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
        ("records.segment", false, false),
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

/// The report a child build printed when it finished.
fn child_report(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    stderr
        .lines()
        .find(|line| line.starts_with("CHILD BUILD FINISHED"))
        .unwrap_or_else(|| panic!("the child finished: {stderr}"))
        .to_string()
}

/// A build killed while running from one executable image, then resumed by
/// a process of `exe` with `env` set: whether the resumed build adopted
/// what the killed one left.
fn resumed_after_an_image_change(replace_first: bool, env: &[(&str, &str)]) -> bool {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let exe = bin.path().join("aft-under-test");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let replace: &[(&str, &str)] = if replace_first {
        &[("AFT_TEST_REPLACE_EXE", "1")]
    } else {
        &[]
    };
    killed_build_of(&exe, store.path(), root.path(), "stage.sites", replace);
    let report = child_report(
        &child(&exe, store.path(), root.path())
            .envs(env.iter().copied())
            .output()
            .unwrap(),
    );
    let fresh = report.contains("resumed_records: 0,") && report.contains("resumed_passes: false");
    let resumed =
        !report.contains("resumed_records: 0,") && report.contains("resumed_passes: true");
    assert!(
        fresh || resumed,
        "either all or nothing was resumed: {report}"
    );
    resumed
}

/// The killed build replaces the binary at its path while it runs, as an
/// upgrade does; the next build runs the binary now at that path. Taking
/// the image's identity from that path at build time, rather than from the
/// image the process started from, would give both builds the same one.
#[test]
fn a_build_never_resumes_what_another_executable_image_wrote() {
    assert!(
        resumed_after_an_image_change(false, &[]),
        "the same image resumes"
    );
    assert!(
        !resumed_after_an_image_change(true, &[]),
        "the replacing image's work is not resumed"
    );
}

/// As [`a_build_never_resumes_what_another_executable_image_wrote`], with
/// the next build replacing the binary at its path in turn before it builds.
#[test]
fn a_build_never_resumes_what_another_executable_image_wrote_when_both_replace() {
    assert!(!resumed_after_an_image_change(
        true,
        &[("AFT_TEST_REPLACE_EXE", "1")]
    ));
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
fn a_text_only_publication_keeps_a_dead_build_s_workspace() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    // The killed build published the text index first.
    killed_build(store.path(), root.path(), "stage.sites");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    assert!(checkout.needs_build());
    let dir = builds(&checkout);
    checkout.discard_build_workspace();
    assert_eq!(
        workspaces(&dir).len(),
        1,
        "kept while a cold build is needed"
    );
    let report = checkout.build().unwrap();
    assert!(report.resumed_passes, "{report:?}");
}

/// Every file of a finished build's workspace is closed before the
/// workspace is deleted (Windows may keep listing a deleted file another
/// handle has open, so the folder would stay).
#[cfg(target_os = "linux")]
#[test]
fn a_build_closes_its_workspace_files_before_deleting_it() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let (checkout, _) = build(store.path(), root.path());
    let dir = builds(&checkout);
    let noted = super::resume::OPEN_AT_FINISH
        .lock()
        .unwrap()
        .iter()
        .filter(|(workspace, _)| workspace.starts_with(&dir))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(noted.len(), 1, "{noted:?}");
    assert!(noted[0].1.is_empty(), "open when deleted: {:?}", noted[0].1);
}

#[test]
fn a_build_of_a_checkout_another_build_holds_is_busy() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let held = checkout
        .family
        .try_write()
        .unwrap()
        .unwrap()
        .claim_scratch(checkout.checkout)
        .unwrap();
    let error = checkout.build().unwrap_err();
    assert!(matches!(error, DataError::Busy(_)), "{error:?}");
    drop(held);
    checkout.build().unwrap();
}

/// Inputs that are not parsed files but that resolution reads: a
/// configuration file's bytes, a symlink's target and the set of paths.
#[test]
fn inputs_besides_parsed_files_changed_between_kill_and_resume_are_resolved_again() {
    let changes: [(&str, fn(&Path)); 3] = [
        ("a configuration file's bytes", |root| {
            write(root, "package.json", "{\"name\": \"renamed\"}\n");
        }),
        // Both targets hold the same bytes: only the link changes.
        ("a symlink's target", |root| {
            std::fs::remove_file(root.join("gen/link.txt")).unwrap();
            std::os::unix::fs::symlink("b.txt", root.join("gen/link.txt")).unwrap();
        }),
        ("a path that is not parsed", |root| {
            std::fs::rename(root.join("notes.txt"), root.join("gen/notes.txt")).unwrap();
        }),
    ];
    for (what, change) in changes {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        write(root.path(), "gen/a.txt", "same\n");
        write(root.path(), "gen/b.txt", "same\n");
        std::os::unix::fs::symlink("a.txt", root.path().join("gen/link.txt")).unwrap();
        let store = tempfile::tempdir().unwrap();
        killed_build(store.path(), root.path(), "passes.committed");
        change(root.path());
        let (resumed, report) = build(store.path(), root.path());
        assert!(report.resumed_records > 0, "{what}: {report:?}");
        assert!(!report.resumed_passes, "{what}: {report:?}");
        let reference_store = tempfile::tempdir().unwrap();
        let (reference, _) = build(reference_store.path(), root.path());
        assert_same(&published(&reference), &published(&resumed), what);
    }
}

/// Each kind of damage to a checkpoint drops what it damaged, and the
/// resumed build still publishes what a fresh build does.
#[test]
fn damaged_or_torn_checkpoints_are_built_again_to_the_same_publication() {
    fn flip(path: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        std::fs::write(path, bytes).unwrap();
    }
    fn tear(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        std::fs::write(path, &bytes[..bytes.len() / 2]).unwrap();
    }
    fn first(workspace: &Path, folder: &str, suffix: &str) -> PathBuf {
        files_ending(&workspace.join(folder), suffix)
            .into_iter()
            .find(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("records-") || name.starts_with("resolutions-")
            })
            .unwrap()
    }
    // Each damage, with whether the records and passes 2-3 are reused.
    let cases: [(&str, fn(&Path, &Path), bool, bool); 7] = [
        (
            "a torn staging segment",
            |w, _| tear(&first(w, "records", ".seg")),
            false,
            true,
        ),
        (
            "a changed staging segment",
            |w, _| flip(&first(w, "records", ".seg")),
            false,
            true,
        ),
        (
            "a changed records table",
            |w, _| flip(&first(w, "records", ".keys")),
            false,
            true,
        ),
        (
            "a torn records marker",
            |w, _| tear(&w.join("STAGE-records")),
            false,
            true,
        ),
        (
            "a changed passes marker",
            |w, _| flip(&w.join("STAGE-passes-2-3")),
            true,
            false,
        ),
        (
            "a torn spooled range",
            |w, _| tear(&first(w, "runs", "")),
            true,
            false,
        ),
        (
            "an unreadable attempts record",
            |_, dir| std::fs::write(dir.join("attempts"), b"torn").unwrap(),
            false,
            false,
        ),
    ];
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    let expected = published(&reference);
    drop(reference);
    for (what, damage, records, passes) in cases {
        let store = tempfile::tempdir().unwrap();
        killed_build(store.path(), root.path(), "stage.sites");
        let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
        let dir = builds(&checkout);
        damage(&only_workspace(&dir), &dir);
        let report = checkout.build().unwrap();
        assert_eq!(report.resumed_records > 0, records, "{what}: {report:?}");
        assert_eq!(report.resumed_passes, passes, "{what}: {report:?}");
        assert_same(&expected, &published(&checkout), what);
    }
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

/// A store with two checkouts, `own` and `other`, and a killed build of
/// `other`, after one of `own` too when `own_killed`. Returns `other`'s
/// dead workspace.
fn killed_builds(store: &Path, own: &Path, own_killed: bool, other: &Path) -> PathBuf {
    project(own);
    if own_killed {
        killed_build(store, own, "stage.sites");
    }
    project(other);
    killed_build(store, other, "stage.sites");
    let checkout = Checkout::bind(layer(), store, family(), other).unwrap();
    only_workspace(&builds(&checkout))
}

/// The publication of a fresh build of `own` in a store where `other`'s
/// build was killed: as `own` here first publishes from `other`'s text
/// index (a near-identical sibling), so does the reference.
fn reference_beside(own: &Path, other: &Path) -> Vec<(String, Vec<u8>)> {
    let store = tempfile::tempdir().unwrap();
    killed_build(store.path(), other, "stage.sites");
    let (reference, _) = build(store.path(), own);
    published(&reference)
}

/// Capacity the free-space probes below report: its floor is 5%, 50 GiB.
const CAPACITY: u64 = 1 << 40;

/// Below the floor until `workspace` is deleted, then plenty: the dead
/// workspace of another checkout holds what this build needs.
fn short_until_deleted(store: &Path, workspace: PathBuf, short: u64) -> impl Drop {
    let floor = crate::disk_floor::free_disk_floor(CAPACITY);
    crate::disk_floor::set_free_space_fn_for_test(store, move || {
        let free = if workspace.exists() {
            short
        } else {
            floor + (64 << 30)
        };
        (free, CAPACITY)
    })
}

/// Below the floor, another checkout's newer dead workspace goes before
/// this checkout's older one, which the build then resumes from.
#[test]
fn below_the_floor_other_checkouts_dead_workspaces_go_before_the_own() {
    let root = tempfile::tempdir().unwrap();
    let (own, other) = (root.path().join("own"), root.path().join("other"));
    let store = tempfile::tempdir().unwrap();
    let theirs = killed_builds(store.path(), &own, true, &other);
    let floor = crate::disk_floor::free_disk_floor(CAPACITY);
    let _low = short_until_deleted(store.path(), theirs.clone(), floor - 1);
    let (resumed, report) = build(store.path(), &own);
    assert!(
        !theirs.exists(),
        "the other checkout's workspace was deleted"
    );
    assert!(
        report.resumed_records > 0 && report.resumed_passes,
        "{report:?}"
    );
    assert_same(
        &reference_beside(&own, &other),
        &published(&resumed),
        "after reclaiming",
    );
}

/// Free space above the floor but short of what a stage asks to write is
/// recovered from another checkout's dead workspace, and the build runs.
/// With nothing to resume, the scan's own writes are the first to ask.
#[test]
fn free_space_just_above_the_floor_is_recovered_from_dead_workspaces() {
    let root = tempfile::tempdir().unwrap();
    let (own, other) = (root.path().join("own"), root.path().join("other"));
    let store = tempfile::tempdir().unwrap();
    let theirs = killed_builds(store.path(), &own, false, &other);
    let floor = crate::disk_floor::free_disk_floor(CAPACITY);
    let _low = short_until_deleted(store.path(), theirs.clone(), floor + 4096);
    let (resumed, _) = build(store.path(), &own);
    assert!(
        !theirs.exists(),
        "the other checkout's workspace was deleted"
    );
    assert_same(
        &reference_beside(&own, &other),
        &published(&resumed),
        "after reclaiming",
    );
}

/// A resumed scan that staged nothing new keeps the records marker it
/// adopted rather than writing it again.
#[test]
fn a_resumed_scan_with_nothing_new_keeps_the_records_marker() {
    let root = tempfile::tempdir().unwrap();
    project(root.path());
    let store = tempfile::tempdir().unwrap();
    // Killed during passes 2-3, then during assembly: two deaths at
    // different stages, so the last build still resumes.
    killed_build(store.path(), root.path(), "passes.range:1");
    let checkout = Checkout::bind(layer(), store.path(), family(), root.path()).unwrap();
    let marker = only_workspace(&builds(&checkout)).join("STAGE-records");
    let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30);
    std::fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    killed_build(store.path(), root.path(), "stage.write");
    assert_eq!(
        std::fs::metadata(&marker).unwrap().modified().unwrap(),
        long_ago,
        "the marker was not written again"
    );
    let report = checkout.build().unwrap();
    assert!(
        report.resumed_records > 0 && report.resumed_passes,
        "{report:?}"
    );
    let reference_store = tempfile::tempdir().unwrap();
    let (reference, _) = build(reference_store.path(), root.path());
    assert_same(
        &published(&reference),
        &published(&checkout),
        "after keeping the marker",
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
