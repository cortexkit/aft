//! Edits that change what a file resolves to without changing the file:
//! each update must give exactly what a cold resolution of the new checkout
//! gives, answers and recorded reads alike.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use aft_store::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence};
use aft_store::content::RecordBody;
use aft_store::ids::{ContentKey, Digest128};

use super::callgraph::{resolve_checkout, CheckoutInputs};
use super::content::{blob_from_record, extract, language_for};
use super::incremental::LiveCheckout;
use crate::callgraph_store::join::{CallgraphBlob, ResolutionStatus};

/// A checkout of text `files` and symlinks `links` (path, target).
pub(super) fn checkout(files: &[(&str, &str)], links: &[(&str, &str)]) -> CheckoutInputs {
    let mut records = Vec::new();
    let mut parses = BTreeMap::new();
    let mut configs = HashMap::new();
    for (path, text) in files {
        let bytes = text.as_bytes();
        let language = language_for(path, bytes).unwrap_or_default();
        records.push(PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o100_644,
            kind: PathKind::Text,
            content: ContentKey::of(bytes),
            link: Vec::new(),
            stat: StatEvidence::default(),
            language: language.clone(),
        });
        if language.is_empty() {
            continue;
        }
        let record = extract(bytes, &language).unwrap();
        match &record.data {
            RecordBody::Config { source } => {
                configs.insert(path.as_bytes().to_vec(), Arc::from(source.as_slice()));
            }
            RecordBody::Parsed { .. } => {
                let CallgraphBlob::Parse(parse) = blob_from_record(&record).unwrap() else {
                    unreachable!("a parsed record converts to a parse");
                };
                let key = Digest128::of(bytes).to_string();
                parses.insert((*path).to_string(), (key, Arc::new(parse)));
            }
        }
    }
    for (path, target) in links {
        records.push(PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o120_000,
            kind: PathKind::Symlink,
            content: ContentKey::of(target.as_bytes()),
            link: target.as_bytes().to_vec(),
            stat: StatEvidence::default(),
            language: String::new(),
        });
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    CheckoutInputs {
        state: CheckoutState::new(Vec::new(), Digest128::of(b""), records).unwrap(),
        parses: parses.into(),
        configs,
    }
}

/// Updates a live resolution of `before` to `after`, whose `changed` paths
/// differ, and checks it against a cold resolution of `after`. Returns the
/// cold resolution's rows of `caller` as (status, target path, symbol).
fn update_matches_cold(
    before: CheckoutInputs,
    after: CheckoutInputs,
    changed: &[&str],
    caller: &str,
) -> Vec<(ResolutionStatus, Option<String>, Option<String>)> {
    update_checked(before, after, changed).1[caller]
        .rows
        .iter()
        .map(|row| {
            (
                row.status,
                row.target_path
                    .as_ref()
                    .map(|path| String::from_utf8_lossy(path).into_owned()),
                row.target_symbol.clone(),
            )
        })
        .collect()
}

/// Updates a live resolution of `before` to `after` and checks it against a
/// cold resolution of `after`, returning the update's counts and the cold
/// resolution.
fn update_checked(
    before: CheckoutInputs,
    after: CheckoutInputs,
    changed: &[&str],
) -> (
    super::incremental::UpdateStats,
    BTreeMap<String, super::callgraph::FileResolution>,
) {
    let mut live = LiveCheckout::cold(before).unwrap();
    let changed = changed
        .iter()
        .map(|path| (*path).to_string())
        .collect::<BTreeSet<_>>();
    let stats = live.update(after, &changed).unwrap();
    // What the update resolved is kept encoded: let go of every decoded
    // copy, so the comparison below reads each back from its encoding.
    live.resolved.release_decoded();
    assert_eq!(live.resolved.decoded_count(), 0);
    let cold = resolve_checkout(&live.inputs, 1).unwrap();
    assert_eq!(
        live.resolved.keys().collect::<Vec<_>>(),
        cold.keys().collect::<Vec<_>>()
    );
    for (path, ours) in live.resolved.iter() {
        let theirs = &cold[path];
        assert_eq!(ours.rows, theirs.rows, "{path}: rows");
        assert_eq!(ours.sites, theirs.sites, "{path}: method-call sites");
        assert_eq!(ours.dispatch, theirs.dispatch, "{path}: dispatch targets");
        assert_eq!(ours.binding, theirs.binding, "{path}: recorded reads");
    }
    (stats, cold)
}

#[test]
fn what_an_update_resolves_is_encoded_and_reads_back_the_same() {
    let caller = (
        "main.ts",
        "import { run } from './lib';\nexport function main() { run(); }\n",
    );
    let before = checkout(
        &[caller, ("lib.ts", "export function run() { return 1; }\n")],
        &[],
    );
    let after = checkout(
        &[
            caller,
            (
                "lib.ts",
                "export function run() { return 2; }\nexport function stop() {}\n",
            ),
        ],
        &[],
    );
    let mut live = LiveCheckout::cold(before).unwrap();
    // Built in memory by a cold build, nothing can be let go.
    assert_eq!(live.resolved.decoded_count(), 0);
    let stats = live
        .update(after, &BTreeSet::from(["lib.ts".to_string()]))
        .unwrap();
    assert!(stats.touched.contains("lib.ts"), "{stats:?}");
    let decoded = live.resolved.to_map();
    // The update's own resolutions are encoded, so each can be let go.
    assert!(live.resolved.decoded_count() >= 1);
    live.resolved.release_decoded();
    assert_eq!(live.resolved.decoded_count(), 0);
    assert_eq!(live.resolved.to_map(), decoded);
    // Changed again, an entry stays decoded until the next update ends.
    live.resolved.get_mut("lib.ts").unwrap().sites.clear();
    live.resolved.release_decoded();
    assert!(live.resolved.get("lib.ts").unwrap().sites.is_empty());
}

/// Every engine resolves on the process's one engine pool, instead of
/// starting a pool of its own whose threads live as long as it does: with
/// dozens of edited worktrees, those were hundreds of idle threads.
#[test]
fn every_engine_resolves_on_the_one_shared_pool() {
    let files = [("lib.ts", "export function run() { return 1; }\n")];
    let first = LiveCheckout::cold(checkout(&files, &[])).unwrap();
    let second = LiveCheckout::cold(checkout(&files, &[])).unwrap();
    assert!(first.shares_pool_with(&second));
    let cores = std::thread::available_parallelism().map_or(4, usize::from);
    assert_eq!(first.install(rayon::current_num_threads), cores);
}

#[test]
fn a_symlink_retarget_resolves_like_a_cold_build() {
    let files = [
        (
            "main.ts",
            "import { run } from './alias/lib';\nexport function main() { run(); }\n",
        ),
        ("old/lib.ts", "export function run() { return 1; }\n"),
        ("new/lib.ts", "export function run() { return 2; }\n"),
    ];
    let rows = update_matches_cold(
        checkout(&files, &[("alias", "old")]),
        checkout(&files, &[("alias", "new")]),
        &["alias"],
        "main.ts",
    );
    // The fixture must really move the call, or the test proves nothing.
    assert!(
        rows.iter()
            .any(|(_, path, _)| path.as_deref() == Some("new/lib.ts")),
        "{rows:?}"
    );
}

#[test]
fn a_workspace_package_rename_resolves_like_a_cold_build() {
    // The caller imports a package name no workspace member has until the
    // member's package.json is renamed: the miss must be recorded, or the
    // rename never reaches the caller.
    let caller = (
        "packages/app/src/main.ts",
        "import { target } from '@scope/new';\nexport function run(): string { return target(); }\n",
    );
    let library = (
        "packages/pkg/src/index.ts",
        "export function target(): string { return 'ok'; }\n",
    );
    let root = ("package.json", r#"{"workspaces":["packages/*"]}"#);
    let app = ("packages/app/package.json", r#"{"name":"app"}"#);
    let before = checkout(
        &[
            caller,
            library,
            root,
            app,
            ("packages/pkg/package.json", r#"{"name":"@scope/old"}"#),
        ],
        &[],
    );
    let after = checkout(
        &[
            caller,
            library,
            root,
            app,
            ("packages/pkg/package.json", r#"{"name":"@scope/new"}"#),
        ],
        &[],
    );
    let rows = update_matches_cold(
        before,
        after,
        &["packages/pkg/package.json"],
        "packages/app/src/main.ts",
    );
    assert!(
        rows.iter()
            .any(|(_, path, _)| path.as_deref() == Some("packages/pkg/src/index.ts")),
        "the rename must resolve the import: {rows:?}"
    );
}

/// A workspace whose `app` imports `@scope/lib`, among `members` other
/// packages each in its own folder (`packages/m<i>`, named `m<i>`).
fn js_workspace(members: usize, extra_files: &[(&str, &str)]) -> CheckoutInputs {
    let mut owned = vec![
        (
            "package.json".to_string(),
            r#"{"workspaces":["packages/*"]}"#.to_string(),
        ),
        (
            "packages/app/package.json".to_string(),
            r#"{"name":"app"}"#.to_string(),
        ),
        (
            "packages/app/src/main.ts".to_string(),
            "import { target } from '@scope/lib';\nexport function run(): string { return target(); }\n"
                .to_string(),
        ),
        (
            "packages/app/src/local.ts".to_string(),
            "import { run } from './main';\nexport function again(): string { return run(); }\n"
                .to_string(),
        ),
        (
            "packages/lib/package.json".to_string(),
            r#"{"name":"@scope/lib","main":"src/index.ts"}"#.to_string(),
        ),
        (
            "packages/lib/src/index.ts".to_string(),
            "export function target(): string { return 'ok'; }\n".to_string(),
        ),
    ];
    for member in 0..members {
        owned.push((
            format!("packages/m{member}/package.json"),
            format!(r#"{{"name":"m{member}"}}"#),
        ));
        owned.push((
            format!("packages/m{member}/src/index.ts"),
            format!("export function f{member}(): number {{ return {member}; }}\n"),
        ));
    }
    let mut files = owned
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect::<Vec<_>>();
    files.extend_from_slice(extra_files);
    checkout(&files, &[])
}

#[test]
fn a_workspace_package_lookup_records_its_name_not_every_member() {
    // Finding `@scope/lib` reads every member's manifest. Recorded in the
    // importing file, those reads grew with the workspace: in a monorepo of
    // a few dozen packages, about 1,300 per file with a bare import. The
    // importer records the looked-up name's key instead, so its stored
    // resolution is the same size with 2 other members as with 40.
    let recorded = |members: usize| {
        let cold = resolve_checkout(&js_workspace(members, &[]), 1).unwrap();
        assert!(
            cold["packages/app/src/main.ts"].rows.iter().any(|row| row
                .target_path
                .as_deref()
                .is_some_and(|path| path == b"packages/lib/src/index.ts")),
            "the fixture must resolve the import"
        );
        super::fragment::encode(&cold["packages/app/src/main.ts"]).len()
    };
    assert_eq!(recorded(2), recorded(40));
}

#[test]
fn a_new_workspace_package_nothing_imports_rebinds_only_its_own_files() {
    // `main.ts` looks up `@scope/lib`, whose member does not change when a
    // new package appears, so only the new package's file is bound. Keying
    // the whole member list would rebind every file with a bare import.
    let extra = [
        ("packages/new/package.json", r#"{"name":"new"}"#),
        (
            "packages/new/src/index.ts",
            "export function fresh(): number { return 1; }\n",
        ),
    ];
    let (stats, _) = update_checked(
        js_workspace(3, &[]),
        js_workspace(3, &extra),
        &["packages/new/package.json", "packages/new/src/index.ts"],
    );
    assert_eq!(stats.rebound, 1, "{stats:?}");
}

#[test]
fn a_second_member_taking_a_package_name_rebinds_its_importer() {
    // A member folder that sorts before `lib` and takes its name becomes the
    // package the import resolves to: the name's key is set, and the
    // importer resolves like a cold build.
    let extra = [
        (
            "packages/alt/package.json",
            r#"{"name":"@scope/lib","main":"src/index.ts"}"#,
        ),
        (
            "packages/alt/src/index.ts",
            "export function target(): string { return 'alt'; }\n",
        ),
    ];
    let rows = update_matches_cold(
        js_workspace(3, &[]),
        js_workspace(3, &extra),
        &["packages/alt/package.json", "packages/alt/src/index.ts"],
        "packages/app/src/main.ts",
    );
    assert!(
        rows.iter()
            .any(|(_, path, _)| path.as_deref() == Some("packages/alt/src/index.ts")),
        "the fixture must move the import: {rows:?}"
    );
}

#[test]
fn a_go_module_rename_resolves_like_a_cold_build() {
    let caller = (
        "app/main.go",
        "package main\n\nimport \"example.com/pkg\"\n\nfunc main() { pkg.Run() }\n",
    );
    let library = ("lib/lib.go", "package pkg\n\nfunc Run() {}\n");
    let app_module = ("app/go.mod", "module example.com/app\n");
    let before = checkout(
        &[
            caller,
            library,
            app_module,
            ("lib/go.mod", "module example.com/pkg\n"),
        ],
        &[],
    );
    let after = checkout(
        &[
            caller,
            library,
            app_module,
            ("lib/go.mod", "module example.org/renamed\n"),
        ],
        &[],
    );
    // The fixture must change an answer: the import resolves before the
    // rename and not after.
    let cold_before = resolve_checkout(&before, 1).unwrap();
    let resolved = |rows: &[crate::callgraph_store::join::DerivedRow]| {
        rows.iter()
            .filter(|row| row.status == ResolutionStatus::Resolved)
            .count()
    };
    let after_rows = update_matches_cold(before, after, &["lib/go.mod"], "app/main.go");
    let after_resolved = after_rows
        .iter()
        .filter(|(status, _, _)| *status == ResolutionStatus::Resolved)
        .count();
    assert!(
        resolved(&cold_before["app/main.go"].rows) > after_resolved,
        "the rename did not change what the caller resolves to"
    );
}

/// A workspace of `app`, which calls `dep::run` and `newdep::go`, and `dep`,
/// plus `extra_files`. No crate is called `newdep` until `extra_files`
/// adds one.
fn workspace(extra_files: &[(&str, &str)]) -> CheckoutInputs {
    let mut files = vec![
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        ),
        (
            "app/src/lib.rs",
            "pub fn start() {\n    dep::run();\n    newdep::go();\n}\n",
        ),
        ("dep/Cargo.toml", "[package]\nname = \"dep\"\n"),
        ("dep/src/lib.rs", "pub fn run() {}\n"),
    ];
    files.extend_from_slice(extra_files);
    checkout(&files, &[])
}

#[test]
fn a_new_crate_nothing_names_rebinds_only_its_own_files() {
    // `app` looks up `dep` and `newdep`; neither entry changes when `extra`
    // appears, so the update binds only the new crate's file. Recording the
    // whole crate map as one key would rebind `app` too.
    let extra = [
        ("extra/Cargo.toml", "[package]\nname = \"extra\"\n"),
        ("extra/src/lib.rs", "pub fn more() {}\n"),
    ];
    let (stats, cold) = update_checked(
        workspace(&[]),
        workspace(&extra),
        &["extra/Cargo.toml", "extra/src/lib.rs"],
    );
    assert_eq!(stats.rebound, 1, "{stats:?}");
    assert!(cold["app/src/lib.rs"].rows.iter().any(|row| row
        .target_path
        .as_deref()
        .is_some_and(|path| path == b"dep/src/lib.rs")));
}

#[test]
fn a_cargo_manifest_added_elsewhere_rebinds_only_the_files_under_it() {
    // `app` resolves `crate::` and `mod` paths, which looks up its crate
    // root through `app/Cargo.toml`, and `web` finds its `package.json` and
    // `tsconfig.json`. A manifest added for an unrelated folder changes no
    // answer either recorded, so neither is bound or resolved again; one key
    // for every config file added or removed would reach both.
    let base = [
        ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\n"),
        ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
        (
            "app/src/lib.rs",
            "mod inner;\nuse crate::inner::help;\npub fn start() {\n    help();\n    crate::inner::help();\n}\n",
        ),
        ("app/src/inner.rs", "pub fn help() {}\n"),
        ("tools/src/lib.rs", "pub fn tool() {}\n"),
        ("web/package.json", r#"{"name":"web"}"#),
        ("web/tsconfig.json", "{}"),
        (
            "web/src/a.ts",
            "import { b } from './b';\nexport function a() { b(); }\n",
        ),
        ("web/src/b.ts", "export function b() {}\n"),
    ];
    let mut after = base.to_vec();
    after.push(("tools/Cargo.toml", "[package]\nname = \"tools\"\n"));
    let (stats, cold) = update_checked(
        checkout(&base, &[]),
        checkout(&after, &[]),
        &["tools/Cargo.toml"],
    );
    assert!(
        cold["app/src/lib.rs"].rows.iter().any(|row| row
            .target_path
            .as_deref()
            .is_some_and(|path| path == b"app/src/inner.rs")),
        "the fixture must resolve the crate path"
    );
    for untouched in ["app/src/lib.rs", "web/src/a.ts"] {
        assert!(!stats.touched.contains(untouched), "{untouched}: {stats:?}");
    }
}

#[test]
fn a_new_crate_a_file_already_names_reaches_that_file() {
    // `app` calls `newdep::go`, which no crate defines until `newdep`
    // appears: the miss must be recorded, or the new crate never reaches
    // `app`.
    let rows = update_matches_cold(
        workspace(&[]),
        workspace(&[
            ("newdep/Cargo.toml", "[package]\nname = \"newdep\"\n"),
            ("newdep/src/lib.rs", "pub fn go() {}\n"),
        ]),
        &["newdep/Cargo.toml", "newdep/src/lib.rs"],
        "app/src/lib.rs",
    );
    assert!(
        rows.iter()
            .any(|(_, path, _)| path.as_deref() == Some("newdep/src/lib.rs")),
        "the new crate must resolve the call: {rows:?}"
    );
}

#[test]
fn a_crate_src_symlink_retarget_resolves_like_a_cold_build() {
    // `dep`'s sources sit behind a link, so the crate map finds them only by
    // following it; retargeting the link moves `dep::run`.
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"app\", \"dep\"]\n"),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        ),
        ("app/src/lib.rs", "pub fn start() { dep::run(); }\n"),
        ("dep/Cargo.toml", "[package]\nname = \"dep\"\n"),
        ("old/src/lib.rs", "pub fn run() {}\n"),
        ("new/src/lib.rs", "pub fn run() {}\n"),
    ];
    let rows = update_matches_cold(
        checkout(&files, &[("dep/src", "../old/src")]),
        checkout(&files, &[("dep/src", "../new/src")]),
        &["dep/src"],
        "app/src/lib.rs",
    );
    assert!(
        rows.iter()
            .any(|(_, path, _)| path.as_deref() == Some("new/src/lib.rs")),
        "the fixture must move the call: {rows:?}"
    );
}
