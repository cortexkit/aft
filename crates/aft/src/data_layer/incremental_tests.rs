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
fn checkout(files: &[(&str, &str)], links: &[(&str, &str)]) -> CheckoutInputs {
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
        parses,
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
    let mut live = LiveCheckout::cold(before, 1).unwrap();
    let changed = changed
        .iter()
        .map(|path| (*path).to_string())
        .collect::<BTreeSet<_>>();
    live.update(after, &changed).unwrap();
    let cold = resolve_checkout(&live.inputs, 1).unwrap();
    assert_eq!(
        live.resolved.keys().collect::<Vec<_>>(),
        cold.keys().collect::<Vec<_>>()
    );
    for (path, ours) in &live.resolved {
        let theirs = &cold[path];
        assert_eq!(ours.rows, theirs.rows, "{path}: rows");
        assert_eq!(ours.sites, theirs.sites, "{path}: method-call sites");
        assert_eq!(ours.dispatch, theirs.dispatch, "{path}: dispatch targets");
        assert_eq!(ours.binding, theirs.binding, "{path}: recorded reads");
    }
    cold[caller]
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
