/// A configured context over a small Rust crate, with its own storage.
fn context(root: &Path, storage: &Path) -> AppContext {
    context_with(root, storage, |_| {})
}

/// As [`context`], with `edit` applied to its configuration.
fn context_with(
    root: &Path,
    storage: &Path,
    edit: impl FnOnce(&mut crate::config::Config),
) -> AppContext {
    let config = root_fixture_config(root, storage, edit);
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_canonical_cache_root(root.to_path_buf());
    ctx
}

/// Pauses the initial build after publication but before it starts a
/// meaning fill. Published planes alone do not mean the build job ended.
/// Sending on, or dropping, the returned sender lets the build continue.
fn context_before_automatic_fill(
    root: &Path,
    storage: &Path,
) -> (AppContext, std::sync::mpsc::Sender<()>) {
    let held_build = crate::data_layer::runtime::hold_builds_for_test(root);
    let ctx = context(root, storage);
    let checkout = ctx.data_checkout().unwrap();
    let (published, publication) = std::sync::mpsc::channel();
    let (resume, continuation) = std::sync::mpsc::channel();
    let continuation = std::sync::Mutex::new(Some(continuation));
    checkout.on_status_change(std::sync::Arc::new(move || {
        let continuation = continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(continuation) = continuation {
            let _ = published.send(());
            let _ = continuation.recv();
        }
    }));
    drop(held_build);
    publication
        .recv_timeout(Duration::from_secs(60))
        .expect("the initial build did not publish");
    (ctx, resume)
}

#[test]
fn an_idle_release_drops_the_checkout_and_its_next_use_serves_the_publication() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = context(&root, storage.path());
    assert!(ctx.data_checkout().is_some());
    settled(&ctx);

    // The reaper's release after `idle.checkout_hours` without a call.
    assert!(ctx.try_release_idle_checkout_views());
    assert!(matches!(
        ctx.probe_data_checkout(),
        Some(DataCheckoutProbe::NotBound)
    ));
    // The state stayed on disk: the next use binds it and answers at
    // once, without a build.
    let checkout = ctx.data_checkout().unwrap();
    assert!(checkout.view().is_some());
    assert!(!checkout.checkout().needs_build());
}

#[test]
fn roots_configure_leaves_unindexed_and_switched_off_indexes_bind_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = context(&root, storage.path());
    let plane = |ctx: &AppContext, key: &str| {
        let payload = ctx.build_status_snapshot();
        (
            payload[key]["status"].as_str().unwrap().to_string(),
            payload[key]["reason"].as_str().map(str::to_string),
        )
    };
    let unavailable = |reason: &str| (UNAVAILABLE.to_string(), Some(reason.to_string()));
    let off = |reason: Option<&str>| (OFF.to_string(), reason.map(str::to_string));

    // The home folder and a filesystem root, as configure marks them.
    ctx.set_degraded_reasons(vec!["home_root".to_string()]);
    assert!(ctx.data_checkout().is_none());
    assert!(matches!(
        ctx.probe_data_checkout(),
        Some(DataCheckoutProbe::Withheld(Withheld::Home))
    ));
    assert_eq!(plane(&ctx, "search_index"), unavailable(reason::HOME_ROOT));
    assert_eq!(
        plane(&ctx, "callgraph_store"),
        unavailable(reason::HOME_ROOT)
    );
    ctx.set_degraded_reasons(vec!["root_too_broad".to_string()]);
    assert!(ctx.data_checkout().is_none());
    assert_eq!(
        plane(&ctx, "callgraph_store"),
        off(Some(reason::ROOT_TOO_BROAD))
    );
    assert!(
        !storage.path().join("data").exists(),
        "no store was opened for either"
    );

    // Both indexes switched off: nothing to build.
    let none = context_with(&root, storage.path(), |config| {
        config.indexes.trigram = false;
        config.indexes.callgraph = false;
    });
    assert!(none.data_checkout().is_none());
    assert_eq!(plane(&none, "search_index"), off(None));
    assert!(!storage.path().join("data").exists());

    // The text index alone switched off: the call graph is built, and
    // grep gets no view, so it walks the folder.
    let graph_only = context_with(&root, storage.path(), |config| {
        config.indexes.trigram = false;
    });
    assert!(graph_only.data_checkout().is_some());
    let settled_graph = {
        let started = Instant::now();
        loop {
            let report = report(&graph_only).unwrap();
            if report.callgraph.status == READY || started.elapsed() > Duration::from_secs(60) {
                break report;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(
        (settled_graph.text.status, settled_graph.callgraph.status),
        (OFF, READY)
    );
    assert!(graph_only.data_view().is_none());
}

#[test]
fn a_store_that_cannot_open_reports_why_on_status_and_health() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // A file where the storage folder should be: the family store
    // cannot be created under it.
    let blocked = storage.path().join("blocked");
    std::fs::write(&blocked, "").unwrap();
    let ctx = context(&root, &blocked);
    assert!(ctx.data_checkout().is_none());

    let payload = ctx.build_status_snapshot();
    assert_eq!(payload["search_index"]["status"], "unavailable");
    assert_eq!(payload["search_index"]["reason"], "bind_failed");
    assert_eq!(payload["callgraph_store"]["reason"], "bind_failed");
    assert!(payload["data_store"]["error"]
        .as_str()
        .unwrap()
        .contains("index store"));
    let health = ctx.try_health_snapshot(&root);
    assert_eq!(health.search_index.unwrap().status, "unavailable");
    assert_eq!(health.data_store.unwrap().reason, Some(reason::BIND_FAILED));
    assert_eq!(
        ctx.memory_root_snapshot().data_layer.unwrap().counts["bound_checkouts"],
        0
    );
}

/// Once meaning search has the root's semantic configuration, its
/// plane, not today's index, is the root's `semantic_index` in health,
/// as it is on the status tool.
#[test]
fn health_reports_the_meaning_plane_once_it_has_a_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ctx, _resume_build) = context_before_automatic_fill(&root, storage.path());
    let checkout = ctx.data_checkout().unwrap();
    settled(&ctx);
    let starting = Some(crate::data_layer::meaning::STARTING_STAGE.to_string());
    let before = ctx.try_health_snapshot(&root).semantic_index.unwrap();
    assert_ne!(before.stage, starting, "today's index reports until then");

    checkout.configure_meaning_without_fill_for_test(Some(&ctx.config().semantic));
    let health = ctx.try_health_snapshot(&root).semantic_index.unwrap();
    assert_eq!(health.status, BUILDING);
    assert_eq!(health.stage, starting);
    assert_eq!(
        health.fill, None,
        "no fill is running and nothing is counted yet"
    );
    let payload = ctx.build_status_snapshot();
    assert_eq!(payload["semantic_index"]["stage"], json!(starting));
    // The build must not start a model load when its pause is released.
    checkout.configure_meaning_without_fill_for_test(None);
}

/// A build can still start a real fill after its planes report ready.
/// Configuring without a fill only avoids starting one on that call.
#[test]
fn health_reports_the_meaning_fill_started_by_a_published_build() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let _failed_model = crate::data_layer::meaning::fail_model_loads_for_test(
        &root,
        "the test does not load a model",
    );
    let held_model = crate::data_layer::meaning::hold_model_loads_for_test(&root);
    let (ctx, resume_build) = context_before_automatic_fill(&root, storage.path());
    let checkout = ctx.data_checkout().unwrap();
    settled(&ctx);
    checkout.configure_meaning_without_fill_for_test(Some(&ctx.config().semantic));
    assert_eq!(
        ctx.try_health_snapshot(&root).semantic_index.unwrap().fill,
        None
    );

    resume_build.send(()).unwrap();
    let started = Instant::now();
    while !checkout.filling() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the published build did not start its fill"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let health = ctx.try_health_snapshot(&root).semantic_index.unwrap();
    assert_eq!(
        health.stage.as_deref(),
        Some(crate::data_layer::meaning::STARTING_STAGE)
    );
    assert_eq!(
        health.fill,
        Some(SemanticFillHealth {
            filling: Some(true),
            ..SemanticFillHealth::default()
        }),
        "the build queued a real fill before any files were counted"
    );

    let (finished, completion) = std::sync::mpsc::channel();
    checkout.on_status_change(std::sync::Arc::new(move || {
        let _ = finished.send(());
    }));
    checkout.configure_meaning_without_fill_for_test(None);
    drop(held_model);
    while checkout.filling() {
        completion
            .recv_timeout(Duration::from_secs(60))
            .expect("the held fill did not finish");
    }
}

#[test]
fn warm_binds_and_index_publishes_once() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = context(&root, storage.path());

    // `aft index` builds in the foreground, then finds the store current.
    let all = crate::data_layer::checkout::Planes::ALL;
    let first = index_root(&root, storage.path(), all).unwrap();
    assert_eq!(
        first,
        IndexOutcome {
            files: 3,
            parsed_files: 2,
            unparsed_files: 0,
            published: true
        }
    );
    let again = index_root(&root, storage.path(), all).unwrap();
    assert!(!again.published, "nothing changed, nothing published");
    write(&root, "src/extra.rs", "pub fn other() {}\n");
    let changed = index_root(&root, storage.path(), all).unwrap();
    assert_eq!((changed.files, changed.published), (4, true));

    // A standing root that asks only for text search builds no call
    // graph, in a store of its own.
    let text_storage = tempfile::tempdir().unwrap();
    let text_only = index_root(
        &root,
        text_storage.path(),
        crate::data_layer::standing::planes(&[crate::config::IndexKind::Search]),
    )
    .unwrap();
    assert_eq!(
        (text_only.files, text_only.parsed_files, text_only.published),
        (4, 0, true)
    );

    // `aft warmup` binds; the publication `aft index` left serves at once.

    let started = Instant::now();
    loop {
        match warm(&ctx) {
            (WarmState::Ready, WarmState::Ready) => break,
            (WarmState::Failed(error), _) | (_, WarmState::Failed(error)) => {
                panic!("{error}")
            }
            pending => assert!(
                started.elapsed() < Duration::from_secs(60),
                "never warmed: {pending:?}"
            ),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(report(&ctx).unwrap().files, Some(4));
}

/// A segment a newer build wrote into this root's family store, named
/// by a manifest this build reads: the bind refuses it by name, and
/// explicit status, status pushes and health report the refusal as
/// today's stores report theirs.
#[test]
fn a_store_file_a_newer_build_wrote_is_refused_on_every_status_surface() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = context(&root, storage.path());
    assert!(ctx.data_checkout().is_some());
    settled(&ctx);
    assert!(ctx.try_release_idle_checkout_views());

    // Every segment on disk names segment format version 99, as a newer
    // build would write it; the length the manifest published holds.
    let families = std::fs::read_dir(storage.path().join("data"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(families.len(), 1, "{families:?}");
    let mut segments = std::fs::read_dir(families[0].join("segments"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    segments.sort();
    let mut planted = Vec::new();
    for path in &segments {
        let mut bytes = std::fs::read(path).unwrap();
        bytes[8..10].copy_from_slice(&99u16.to_le_bytes());
        std::fs::remove_file(path).unwrap();
        std::fs::write(path, &bytes).unwrap();
        planted.push(bytes);
    }

    assert!(ctx.data_checkout().is_none(), "the bind is refused");
    let Some(DataCheckoutProbe::BindFailed(error)) = ctx.probe_data_checkout() else {
        panic!("the bind failure is kept");
    };
    let refusal = error.refusal().expect("kept typed").clone();
    assert!(segments.contains(&refusal.path), "{refusal:?}");
    assert_eq!(
        (refusal.file, refusal.found, refusal.supported),
        (aft_store::family::FileKind::Segment, 99, 1)
    );
    let message = format!(
            "storage_requires_newer_reader: index_store at {} has format version 99; this build reads up to 1",
            refusal.path.display()
        );
    assert_eq!(error.to_string(), message);
    let entry = json!({
        "code": "storage_requires_newer_reader",
        "store": "index_store",
        "path": refusal.path.display().to_string(),
        "found": 99,
        "supported": 1,
        "message": message,
    });
    let reason = json!("storage_requires_newer_reader:index_store");

    let explicit = ctx.build_status_snapshot();
    assert_eq!(explicit["degraded"], true);
    assert!(
        explicit["degraded_reasons"]
            .as_array()
            .unwrap()
            .contains(&reason),
        "{explicit:#}"
    );
    assert_eq!(explicit["storage_refusals"], json!([entry]));
    for plane in ["search_index", "callgraph_store"] {
        assert_eq!(explicit[plane]["status"], UNAVAILABLE, "{plane}");
        assert_eq!(
            explicit[plane]["reason"],
            reason::REQUIRES_NEWER_READER,
            "{plane}"
        );
    }
    assert_eq!(explicit["data_store"]["error"], message);

    let pushed = ctx.try_build_status_push_snapshot().unwrap();
    assert_eq!(pushed["degraded"], true);
    assert!(
        pushed["degraded_reasons"]
            .as_array()
            .unwrap()
            .contains(&reason),
        "{pushed:#}"
    );
    assert_eq!(pushed["storage_refusals"], json!([entry]));
    assert_eq!(pushed["storage_refusals_total"], 1);
    assert_eq!(pushed["storage_refusals_omitted"], 0);

    let health = ctx.try_health_snapshot(&root).data_store.unwrap();
    assert_eq!(health.reason, Some(reason::REQUIRES_NEWER_READER));
    assert_eq!(health.error.as_deref(), Some(message.as_str()));

    // Nothing was rebuilt over the refused segments or reclaimed.
    let mut after = std::fs::read_dir(families[0].join("segments"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    after.sort();
    assert_eq!(after, segments);
    for (path, bytes) in segments.iter().zip(&planted) {
        assert_eq!(&std::fs::read(path).unwrap(), bytes, "{}", path.display());
    }
}
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::context::AppContext;
use crate::data_layer::status::SemanticFillHealth;
use crate::data_layer::surfaces::{fixtures::*, *};
