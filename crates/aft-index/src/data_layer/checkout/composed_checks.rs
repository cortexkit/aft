pub fn memory_figure_read_side_allocations_advance_the_checkout_epoch(
    entry_points: &dyn EntryPoints,
) {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "src/lib.rs",
        "pub fn used() {}\npub fn unused() {}\n",
    );
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
    checkout.build().unwrap();
    let view = checkout.view().unwrap();
    let epoch = checkout.memory_epoch();
    view.members(&view.published.texts().unwrap());
    assert!(
        checkout.memory_epoch() > epoch,
        "lazy text members changed heap size"
    );
    let epoch = checkout.memory_epoch();
    view.members(&view.published.texts().unwrap());
    assert_eq!(
        checkout.memory_epoch(),
        epoch,
        "reusing members allocates nothing"
    );
    view.dead_code_snapshot_shared(entry_points).unwrap();
    assert!(
        checkout.memory_epoch() > epoch,
        "the dead-code memo was populated"
    );
    let epoch = checkout.memory_epoch();
    assert!(view.release_dead_code_snapshot());
    assert!(
        checkout.memory_epoch() > epoch,
        "the dead-code memo was dropped"
    );
}

pub(super) fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub(super) fn callers_of(view: &CheckoutView, file: &str, symbol: &str) -> String {
    // Resolved callers only: `callers` also lists unresolved call sites
    // that merely share the name, and those would make a negative check
    // ("main.ts is not a caller yet") match on the candidate list.
    let callers = crate::data_layer::callgraph_store_adapter::callers_result(
        &view.callgraph(),
        &view.root().join(file),
        symbol,
        1,
        true,
    )
    .map(|mut result| {
        result.unresolved_callers = None;
        result
    });
    format!("{callers:?}")
}

pub(super) fn family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"demo"))
}

pub fn an_edit_updates_the_call_graph_without_a_rebuild(entry_points: &dyn EntryPoints) {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write(root.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
    write(
        root.path(),
        "src/lib.rs",
        "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
    );
    write(root.path(), "src/util.rs", "pub fn callee() {}\n");
    write(root.path(), "src/extra.rs", "pub fn other() {}\n");
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let family = FamilyId::from_digest(Digest128::of(b"demo"));
    let checkout = Checkout::bind(layer, store.path(), family, root.path()).unwrap();
    checkout.build().unwrap();
    let before = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
    assert!(!before.contains("src/extra.rs"), "{before}");

    // A new caller of `callee` in another file, and the old caller
    // removed: the call graph must show both after a refresh, with no
    // rebuild.
    let extra = "use crate::util;\npub fn other() { util::callee(); }\n";
    let lib = "mod util;\nmod extra;\npub fn caller() {}\n";
    write(root.path(), "src/extra.rs", extra);
    write(root.path(), "src/lib.rs", lib);
    checkout
        .record_paths(&[b"src/extra.rs".to_vec(), b"src/lib.rs".to_vec()])
        .unwrap();
    assert!(matches!(
        checkout.status().callgraph,
        PlaneStatus::Building { .. }
    ));
    let stats = checkout
        .refresh_callgraph()
        .unwrap()
        .expect("two files changed");
    assert!(stats.resolved >= 2, "{stats:?}");
    assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
    let view = checkout.view().unwrap();
    assert_eq!(
        view.changed_paths(),
        vec![b"src/extra.rs".to_vec(), b"src/lib.rs".to_vec()],
        "a refresh resolves the call graph but the paths are still changed since the publication"
    );
    drop(view);
    let after = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
    assert!(after.contains("src/extra.rs"), "{after}");
    assert!(!after.contains("src/lib.rs"), "{after}");

    // Nothing changed since: a second refresh does nothing, and the text
    // generation only moves with a recorded change.
    let generation = checkout.view().unwrap().text_generation();
    assert!(checkout.refresh_callgraph().unwrap().is_none());
    assert_eq!(checkout.view().unwrap().text_generation(), generation);
    // Recording a path whose content did not change changes nothing.
    checkout.record_paths(&[b"src/extra.rs".to_vec()]).unwrap();
    assert_eq!(checkout.view().unwrap().text_generation(), generation);
    let extra_again = "use crate::util;\npub fn other() { util::callee(); util::callee(); }\n";
    write(root.path(), "src/extra.rs", extra_again);
    checkout.record_paths(&[b"src/extra.rs".to_vec()]).unwrap();
    assert_ne!(checkout.view().unwrap().text_generation(), generation);
    checkout.refresh_callgraph().unwrap();

    // The dead-code snapshot follows the refreshed graph: `callee` is
    // still called (from src/extra.rs), and the snapshot equals a cold
    // build's.
    let mut refreshed_snapshot = checkout
        .view()
        .unwrap()
        .rows_dead_code_snapshot(entry_points)
        .unwrap();

    // The answers equal a cold build of the same files.
    let cold_store = tempfile::tempdir().unwrap();
    let cold = Checkout::bind(
        Arc::new(DataLayer::new(8 << 20, 2)),
        cold_store.path(),
        family,
        root.path(),
    )
    .unwrap();
    cold.build().unwrap();
    assert_eq!(
        callers_of(&cold.view().unwrap(), "src/util.rs", "callee"),
        after
    );
    let mut cold_snapshot = cold
        .view()
        .unwrap()
        .rows_dead_code_snapshot(entry_points)
        .unwrap();
    refreshed_snapshot.generated_at = None;
    cold_snapshot.generated_at = None;
    assert_eq!(
        format!("{refreshed_snapshot:?}"),
        format!("{cold_snapshot:?}"),
        "the refreshed dead-code snapshot equals a cold build's"
    );
    assert!(
        refreshed_snapshot
            .outbound_calls
            .iter()
            .any(|call| call.caller_file.ends_with("src/extra.rs")),
        "the new caller is in the snapshot: {refreshed_snapshot:?}"
    );
}

pub fn a_built_checkout_answers_text_and_call_graph_queries_and_survives_a_restart(
    entry_points: &dyn EntryPoints,
) {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write(root.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
    write(
        root.path(),
        "src/lib.rs",
        "mod util;\npub fn caller() { util::callee(); }\n",
    );
    write(root.path(), "src/util.rs", "pub fn callee() {}\n");
    write(root.path(), "notes.txt", "nothing to see\n");
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let family = FamilyId::from_digest(Digest128::of(b"demo"));

    let checkout = Checkout::bind(Arc::clone(&layer), store.path(), family, root.path()).unwrap();
    assert!(
        checkout.view().is_none(),
        "nothing is published before a build"
    );
    let report = checkout.build().unwrap();
    assert_eq!(report.files, 4, "{report:?}");
    assert_eq!(report.parsed_files, 2, "{report:?}");
    assert_eq!(report.extract_failures, 0);

    let check = |view: &CheckoutView| {
        let query = plan_pattern("callee", false).unwrap();
        let found = view.text_candidates(&query, Scope::All).unwrap();
        assert_eq!(found, vec![b"src/lib.rs".to_vec(), b"src/util.rs".to_vec()]);
        let callers = callers_of(view, "src/util.rs", "callee");
        assert!(callers.contains("caller"), "{callers}");
        assert!(callers.contains("src/lib.rs"), "{callers}");
        assert!(view.callgraph_failures().is_empty());
    };
    check(&checkout.view().unwrap());
    assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);

    // The ranking primitives and the dead-code snapshot.
    let view = checkout.view().unwrap();
    let cal = aft_store::text::pack(b'c', b'a', b'l');
    assert_eq!(
        view.paths_with_trigram(cal).unwrap(),
        vec![b"src/lib.rs".to_vec(), b"src/util.rs".to_vec()]
    );
    assert!(view.trigram_frequency(cal).unwrap() >= 2);
    assert!(matches!(
        view.path_text(b"src/lib.rs").unwrap(),
        Some(PathText::Indexed { distinct }) if distinct > 10
    ));
    assert_eq!(view.path_text(b"missing.rs").unwrap(), None);
    let snapshot = view.rows_dead_code_snapshot(entry_points).unwrap();
    assert_eq!(snapshot.files.len(), 2, "{:?}", snapshot.files);
    assert!(
        snapshot
            .outbound_calls
            .iter()
            .any(|call| format!("{call:?}").contains("callee")),
        "{:?}",
        snapshot.outbound_calls
    );
    drop(view);

    // An edit is visible to text queries at once, and the call graph
    // reports it pending until the next build covers it.
    write(root.path(), "notes.txt", "now mentions callee\n");
    checkout.record_paths(&[b"notes.txt".to_vec()]).unwrap();
    let query = plan_pattern("callee", false).unwrap();
    let three = vec![
        b"notes.txt".to_vec(),
        b"src/lib.rs".to_vec(),
        b"src/util.rs".to_vec(),
    ];
    let view = checkout.view().unwrap();
    assert_eq!(view.text_candidates(&query, Scope::All).unwrap(), three);
    assert_eq!(
        checkout.status().callgraph,
        PlaneStatus::Building {
            done: 0,
            total: Some(1)
        }
    );
    drop(view);
    checkout.build().unwrap();
    assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
    let view = checkout.view().unwrap();
    assert!(view.changed_paths().is_empty());
    assert_eq!(
        view.text_candidates(&query, Scope::All).unwrap(),
        three,
        "the rebuild indexed the edit"
    );
    drop(view);

    // A restarted process binds the same checkout and serves the
    // published generation without building.
    drop(checkout);
    let restarted = Checkout::bind(layer, store.path(), family, root.path()).unwrap();
    let view = restarted
        .view()
        .expect("the published view is served at bind");
    assert_eq!(view.text_candidates(&query, Scope::All).unwrap(), three);
    let callers = callers_of(&view, "src/util.rs", "callee");
    assert!(callers.contains("src/lib.rs"), "{callers}");
}

/// How many modules the fold tests' crate has.
const FOLD_MODULES: usize = 4;

/// The fold tests' crate: `top` calls `m0::f0`, each module's function
/// calls the next one's, and each of `added` is a module of its own
/// calling one of them. A binary file and a text file sit beside them.
fn fold_crate(root: &Path, added: &[(usize, usize)]) {
    write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
    fold_lib(root, added);
    for index in 0..FOLD_MODULES {
        fold_module(root, index, "");
    }
    write(root, "notes.txt", "nothing to see\n");
    std::fs::write(root.join("logo.bin"), [0u8, 1, 2, 0, 255, 0, 7]).unwrap();
}

/// The fold tests' `src/lib.rs`, declaring the modules of `added`, and
/// those modules.
fn fold_lib(root: &Path, added: &[(usize, usize)]) {
    let mut lib = String::new();
    for index in 0..FOLD_MODULES {
        lib.push_str(&format!("mod m{index};\n"));
    }
    for (round, index) in added {
        lib.push_str(&format!("mod added{round};\n"));
        write(
            root,
            &format!("src/added{round}.rs"),
            &format!("pub fn added{round}() {{ crate::m{index}::f{index}(); }}\n"),
        );
    }
    lib.push_str("pub fn top() { m0::f0(); }\n");
    write(root, "src/lib.rs", &lib);
}

/// The fold tests' module `index`, with `extra` after its function.
fn fold_module(root: &Path, index: usize, extra: &str) {
    let body = if index + 1 < FOLD_MODULES {
        format!("crate::m{}::f{}();", index + 1, index + 1)
    } else {
        String::new()
    };
    write(
        root,
        &format!("src/m{index}.rs"),
        &format!("pub fn f{index}() {{ {body} }}\n{extra}"),
    );
}

/// What `view` answers that a fold must not change: callers, impact and
/// traces of every function, text candidates, and the dead-code
/// snapshot.
fn fold_answers(entry_points: &dyn EntryPoints, view: &CheckoutView, rounds: usize) -> Vec<String> {
    let mut functions = vec![("src/lib.rs".to_string(), "top".to_string())];
    for index in 0..FOLD_MODULES {
        functions.push((format!("src/m{index}.rs"), format!("f{index}")));
    }
    for round in 0..rounds {
        functions.push((format!("src/added{round}.rs"), format!("added{round}")));
    }
    let mut answers = Vec::new();
    for (file, symbol) in &functions {
        answers.push(format!(
            "{file} {symbol}: {}",
            callers_of(view, file, symbol)
        ));
        for command in ["callers", "impact", "trace_to"] {
            answers.push(format!(
                "{command} {file} {symbol}: {:?}",
                view.probe_query(command, file, symbol)
            ));
        }
    }
    for pattern in ["pub fn", "nothing to see", "crate::m2", "added1", "marker"] {
        answers.push(format!("{pattern}: {:?}", candidates_for(view, pattern)));
    }
    let mut snapshot = view.rows_dead_code_snapshot(entry_points).unwrap();
    snapshot.generated_at = None;
    answers.push(format!("{snapshot:?}"));
    answers
}

/// A checkout that published its changes round after round reads from
/// a segment per round. Folding them copies what it reads into one new
/// segment and retires the rest, without reading a file or resolving
/// anything. It answers exactly as before, and as a cold build of its
/// files does; the store keeps as many segments as a build leaves; a
/// view held across the fold keeps answering from the old segments; a
/// change recorded and not published stays laid over the folded
/// publication; and a restart serves the fold.
pub fn folding_published_changes_answers_as_before_and_as_a_cold_build(
    entry_points: &dyn EntryPoints,
) {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fold_crate(root.path(), &[]);
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let checkout = Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
    checkout.build().unwrap();
    // Each round edits a module and adds a module calling it, which no
    // later round changes, so every round's segment stays read.
    let rounds = COMPACT_AT + 1;
    let mut added = Vec::new();
    let mut report = None;
    for round in 0..rounds {
        let index = round % FOLD_MODULES;
        added.push((round, index));
        fold_lib(root.path(), &added);
        fold_module(root.path(), index, &format!("pub fn r{round}() {{}}\n"));
        checkout
            .record_paths(&[
                b"src/lib.rs".to_vec(),
                format!("src/m{index}.rs").into_bytes(),
                format!("src/added{round}.rs").into_bytes(),
            ])
            .unwrap();
        checkout.refresh_callgraph().unwrap();
        report = checkout.publish_changes().unwrap();
    }
    let report = report.expect("published");
    assert!(report.wants_fold(), "{report:?}");
    let segments_before = live_segments(&checkout);
    // A change recorded and resolved, but not published.
    write(
        root.path(),
        "src/m3.rs",
        "pub fn f3() { marker(); }\npub fn marker() {}\n",
    );
    checkout.record_paths(&[b"src/m3.rs".to_vec()]).unwrap();
    checkout.refresh_callgraph().unwrap();
    let held = checkout.view().unwrap();
    let before = fold_answers(entry_points, &held, rounds);
    assert!(before.iter().any(|answer| answer.contains("marker")));

    let work = source_work_for_test(root.path());
    let Folded::Done(folded) = checkout.fold().unwrap() else {
        panic!("the fold did not finish");
    };
    assert_eq!(
        source_work_for_test(root.path()),
        work,
        "a fold reads no file and resolves nothing"
    );
    assert!(folded.read_before > COMPACT_AT, "{folded:?}");
    assert_eq!(folded.retired_segments, segments_before, "{folded:?}");
    let segments = live_segments(&checkout);
    assert_eq!(segments, 1, "{folded:?}");
    // The unpublished change is still there, and pending again in the
    // call graph until a refresh resolves it over the new publication.
    assert_eq!(checkout.changed_paths(), 1);
    assert_eq!(checkout.view().unwrap().status().callgraph_pending, 1);
    assert!(candidates_for(&checkout.view().unwrap(), "marker").contains(&"src/m3.rs".to_string()));
    checkout.refresh_callgraph().unwrap();
    assert_eq!(
        fold_answers(entry_points, &checkout.view().unwrap(), rounds),
        before
    );
    // The view taken before the fold still reads the old segments.
    assert_eq!(fold_answers(entry_points, &held, rounds), before);
    drop(held);

    // A cold build of the same files answers the same, and writes as
    // many segments.
    let cold_store = tempfile::tempdir().unwrap();
    let cold =
        Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root.path()).unwrap();
    cold.build().unwrap();
    assert_eq!(
        fold_answers(entry_points, &cold.view().unwrap(), rounds),
        before
    );
    assert_eq!(live_segments(&cold), segments);

    // The change publishes over the folded publication as over any.
    checkout.publish_changes().unwrap().expect("published");
    assert_eq!(
        fold_answers(entry_points, &checkout.view().unwrap(), rounds),
        before
    );
    // A restart serves what was published.
    drop(checkout);
    let restarted = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
    assert_eq!(restarted.changed_paths(), 0);
    assert_eq!(
        fold_answers(entry_points, &restarted.view().unwrap(), rounds),
        before
    );
}

/// Publishes `rounds` rounds of changes to `checkout`, the fold tests'
/// crate at `root`: each edits a module and adds a module calling it,
/// so every round's segment stays read. Returns the last report.
fn publish_fold_rounds(checkout: &Checkout, root: &Path, rounds: usize) -> DeltaReport {
    let mut added = Vec::new();
    let mut report = None;
    for round in 0..rounds {
        let index = round % FOLD_MODULES;
        added.push((round, index));
        fold_lib(root, &added);
        fold_module(root, index, &format!("pub fn r{round}() {{}}\n"));
        checkout
            .record_paths(&[
                b"src/lib.rs".to_vec(),
                format!("src/m{index}.rs").into_bytes(),
                format!("src/added{round}.rs").into_bytes(),
            ])
            .unwrap();
        checkout.refresh_callgraph().unwrap();
        report = checkout.publish_changes().unwrap();
    }
    report.expect("published")
}

/// A checkout folds its own segments only after it publishes, so one
/// that stopped publishing kept every segment it reads. Another
/// checkout's fold on its behalf folds them while it stays bound in the
/// same process, with its call-graph engine kept and a change of its own
/// recorded: it reads no file of that checkout, leaves fewer segments,
/// and that checkout, a view of it held across the fold, and its next
/// edit, publication, own fold and restart all answer as a cold build
/// of its files does. With nothing due, nothing is folded.
pub fn a_checkout_that_stopped_publishing_is_folded_on_its_behalf(entry_points: &dyn EntryPoints) {
    let idle_root = tempfile::tempdir().unwrap();
    let busy_root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fold_crate(idle_root.path(), &[]);
    fold_crate(busy_root.path(), &[]);
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let idle =
        Checkout::bind(Arc::clone(&layer), store.path(), family(), idle_root.path()).unwrap();
    idle.build().unwrap();
    let busy =
        Checkout::bind(Arc::clone(&layer), store.path(), family(), busy_root.path()).unwrap();
    busy.build().unwrap();
    let rounds = COMPACT_AT + 1;
    let report = publish_fold_rounds(&idle, idle_root.path(), rounds);
    assert!(report.wants_fold(), "{report:?}");
    // A change recorded and resolved, not published, so the engine is
    // kept and the change laid over the publication.
    write(
        idle_root.path(),
        "src/m3.rs",
        "pub fn f3() { marker(); }\npub fn marker() {}\n",
    );
    idle.record_paths(&[b"src/m3.rs".to_vec()]).unwrap();
    idle.refresh_callgraph().unwrap();
    let held = idle.view().unwrap();
    let before = fold_answers(entry_points, &held, rounds);
    assert!(before.iter().any(|answer| answer.contains("marker")));
    let cold_answers = |rounds: usize| {
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(
            Arc::clone(&layer),
            cold_store.path(),
            family(),
            idle_root.path(),
        )
        .unwrap();
        cold.build().unwrap();
        fold_answers(entry_points, &cold.view().unwrap(), rounds)
    };
    assert_eq!(cold_answers(rounds), before);

    // The other checkout publishes a change of its own, which calls for
    // no fold of its own.
    write(busy_root.path(), "notes.txt", "busy here\n");
    busy.record_paths(&[b"notes.txt".to_vec()]).unwrap();
    busy.refresh_callgraph().unwrap();
    let own = busy.publish_changes().unwrap().expect("published");
    assert!(!own.wants_fold(), "{own:?}");
    let segments_before = live_segments(&busy);
    let work = source_work_for_test(idle_root.path());
    let other = busy.fold_other().unwrap();
    assert_eq!(
        source_work_for_test(idle_root.path()),
        work,
        "a fold on its behalf reads no file of the checkout"
    );
    assert_eq!(other.examined, 1, "{other:?}");
    assert_eq!(
        other
            .root
            .as_deref()
            .map(|root| std::fs::canonicalize(root).unwrap()),
        Some(std::fs::canonicalize(idle_root.path()).unwrap()),
        "{other:?}"
    );
    assert!(other.own_segments > COMPACT_AT, "{other:?}");
    assert_eq!(other.skipped, None);
    let fold = other.fold.clone().expect("folded");
    assert!(fold.retired_segments >= COMPACT_AT, "{other:?}");
    assert!(live_segments(&busy) < segments_before, "{other:?}");

    // A later publication reclaims what the fold retired; the held view
    // and the publication the idle checkout serves keep reading the
    // generation their lease holds.
    write(busy_root.path(), "notes.txt", "busy again\n");
    busy.record_paths(&[b"notes.txt".to_vec()]).unwrap();
    busy.refresh_callgraph().unwrap();
    busy.publish_changes().unwrap().expect("published");
    assert_eq!(fold_answers(entry_points, &held, rounds), before);
    assert_eq!(
        fold_answers(entry_points, &idle.view().unwrap(), rounds),
        before
    );
    drop(held);

    // Edited again and queried, it answers as a cold build of its files.
    write(
        idle_root.path(),
        "src/m2.rs",
        "pub fn f2() { crate::m3::f3(); marker_two(); }\npub fn marker_two() {}\n",
    );
    idle.record_paths(&[b"src/m2.rs".to_vec()]).unwrap();
    idle.refresh_callgraph().unwrap();
    let edited = fold_answers(entry_points, &idle.view().unwrap(), rounds);
    assert_ne!(edited, before);
    assert_eq!(edited, cold_answers(rounds));
    // Its own next publication and its own fold work over the fold.
    idle.publish_changes().unwrap().expect("published");
    assert_eq!(
        fold_answers(entry_points, &idle.view().unwrap(), rounds),
        edited
    );
    let Folded::Done(_) = idle.fold().unwrap() else {
        panic!("the idle checkout's own fold did not finish");
    };
    assert_eq!(
        fold_answers(entry_points, &idle.view().unwrap(), rounds),
        edited
    );
    drop(idle);
    let restarted =
        Checkout::bind(Arc::clone(&layer), store.path(), family(), idle_root.path()).unwrap();
    assert_eq!(restarted.changed_paths(), 0);
    assert_eq!(
        fold_answers(entry_points, &restarted.view().unwrap(), rounds),
        edited
    );

    // The other checkout answers as a cold build of its own files, and
    // with nothing due now, it folds nothing.
    let busy_cold_store = tempfile::tempdir().unwrap();
    let busy_cold = Checkout::bind(
        Arc::clone(&layer),
        busy_cold_store.path(),
        family(),
        busy_root.path(),
    )
    .unwrap();
    busy_cold.build().unwrap();
    assert_eq!(
        fold_answers(entry_points, &busy.view().unwrap(), 0),
        fold_answers(entry_points, &busy_cold.view().unwrap(), 0)
    );
    let none = busy.fold_other().unwrap();
    assert!(none.root.is_none() && none.fold.is_none(), "{none:?}");
}
use super::*;
use crate::data_layer::dead_code_snapshot::EntryPoints;
use aft_store::text::plan_pattern;

fn candidates_for(view: &CheckoutView, pattern: &str) -> Vec<String> {
    let query = plan_pattern(pattern, false).unwrap();
    view.text_candidates(&query, Scope::All)
        .unwrap()
        .into_iter()
        .map(|path| String::from_utf8(path).unwrap())
        .collect()
}

fn live_segments(checkout: &Checkout) -> usize {
    checkout.store_segments().unwrap()
}
