//! Isolated regressions for grammar coverage at command and fill boundaries.

use super::*;
use crate::data_layer::{grammar, runtime::DataCheckout, test_gates};

fn run(name: &str, test: fn()) {
    if std::env::var("AFT_S4_AUDIT_TEST").as_deref() == Ok(name) {
        test();
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("data_layer::grammar_tests::audit::{name}"),
            "--nocapture",
        ])
        .env("AFT_S4_AUDIT_TEST", name)
        .env("AFT_S4_AUDIT_ROOT", dir.path())
        .env(
            "AFT_TEST_GRAMMAR_MANIFEST",
            crate::test_support::manifest_dir().join("tests/fixtures/grammars/manifest-a.json"),
        )
        .env("AFT_TEST_GRAMMAR_CLOCK_FILE", dir.path().join("clock"))
        .env(
            "AFT_TEST_GRAMMAR_TRANSPORT_FILE",
            dir.path().join("transport"),
        )
        .env(
            "AFT_TEST_GRAMMAR_PARSE_DEADLINE_MS",
            if name.contains("timeout") {
                "0"
            } else {
                "30000"
            },
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{name}\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

macro_rules! isolated {
    ($name:ident, $test:expr) => {
        #[test]
        fn $name() {
            run(stringify!($name), $test);
        }
    };
}

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    storage: PathBuf,
    ctx: AppContext,
    registry: Arc<crate::grammar::GrammarRegistry>,
    wasm: Vec<u8>,
}

fn fixture(ready: bool) -> Fixture {
    let dir = PathBuf::from(std::env::var_os("AFT_S4_AUDIT_ROOT").unwrap());
    #[cfg(unix)]
    let dir = std::fs::canonicalize(dir).unwrap();
    let root = dir.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let storage = dir.join("storage");
    write(&dir, "clock", "0");
    let ctx = context(&root, &storage);
    let registry = crate::grammar::configure(&ctx.config());
    let manifest = crate::grammar::manifest();
    let wasm = std::fs::read(
        crate::test_support::manifest_dir()
            .join("tests/fixtures/grammars")
            .join(format!("{}.wasm", manifest.sha256)),
    )
    .unwrap();
    if ready {
        crate::grammar::test_seams::publish_verified_artifact(
            &crate::root_cache::cache_root(&ctx.config()),
            manifest,
            &wasm,
        )
        .unwrap();
        registry.stored().unwrap();
    }
    Fixture {
        dir,
        root,
        storage,
        ctx,
        registry,
        wasm,
    }
}

fn graph_ask(f: &Fixture, params: Value) -> Response {
    use crate::subc_translate::{subc_translate_with_context, TranslateContext};
    let translated = subc_translate_with_context(
        "callgraph",
        &params,
        &f.root,
        TranslateContext {
            diagnostics_on_edit: false,
            preview: false,
            effective_hashline: false,
        },
    )
    .unwrap();
    ask(&f.ctx, &translated.command, Value::Object(translated.args))
}

fn until<T>(mut get: impl FnMut() -> Option<T>, why: &str) -> T {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = get() {
            return value;
        }
        assert!(Instant::now() < until, "{why}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

const SOURCE: &str = "local function source(input)\n return input + 1\nend\n";

isolated!(
    luau_s4_direct_refusals_are_partial_for_every_availability_state,
    refusals
);
fn refusals() {
    use crate::grammar::GrammarState::*;
    let f = fixture(true);
    for (index, state) in [
        SupportDisabled,
        NotDownloaded,
        Downloading,
        NetworkRetry,
        StorageRetry,
        AttemptsExhausted,
        VerificationFailed,
        LoadFailed,
        UnsupportedPlatform,
    ]
    .into_iter()
    .enumerate()
    {
        let forced = f.registry.force_unavailable_for_test(state);
        let root = f.root.join(format!("state-{index}"));
        let path = write(&root, "source.luau", SOURCE);
        let ctx = context(&root, &f.storage);
        for (command, params) in [
            ("outline", json!({"file":path})),
            (
                "zoom",
                json!({"file":path,"symbol":"source","callgraph":true}),
            ),
        ] {
            let response = ask(&ctx, command, params);
            assert!(!response.success, "{command}: {:?}", response.data);
            assert_eq!(response.data["code"], "grammar_unavailable");
            assert_eq!(response.data["state"], json!(state));
            assert_eq!(response.data["complete"], false);
        }
        let runtime = ctx.data_checkout().unwrap();
        runtime.wait_for_memory_profile_idle_for_test();
        let response = ask(&ctx, "callers", json!({"file":path,"symbol":"source"}));
        assert!(!response.success, "{:?}", response.data);
        assert_eq!(response.data["code"], "grammar_unavailable");
        assert_eq!(response.data["state"], json!(state));
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["path"], json!(path));
        let view = runtime.view().unwrap();
        let reason: Value =
            serde_json::from_str(&view.current_state().skipped_parses()[b"source.luau".as_slice()])
                .unwrap();
        assert_eq!(reason.as_object().unwrap().len(), 2);
        assert!(reason.get("retry_after_ms").is_none());
        drop(forced);
    }
}

isolated!(
    luau_s4_direct_timeout_refusals_are_partial_and_name_the_path,
    timeout_refusals
);
fn timeout_refusals() {
    let f = fixture(true);
    let path = write(&f.root, "source.luau", SOURCE);
    for (command, params) in [
        ("outline", json!({"file":path})),
        (
            "zoom",
            json!({"file":path,"symbol":"source","callgraph":true}),
        ),
    ] {
        let response = ask(&f.ctx, command, params);
        assert!(!response.success, "{command}: {:?}", response.data);
        assert_eq!(response.data["code"], "grammar_parse_timeout");
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["path"], json!(path));
    }
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    let response = ask(&f.ctx, "callers", json!({"file":path,"symbol":"source"}));
    assert_eq!(response.data["code"], "grammar_parse_timeout");
    assert_eq!(response.data["complete"], false);
    assert_eq!(response.data["path"], json!(path));
    let mut response = Response::success("timeout", json!({"gaps":[{"kind":"existing"}]}));
    crate::data_layer::surfaces::disclose_parse_gaps(
        &runtime.view().unwrap(),
        &[f.root.clone()],
        &mut response,
    );
    assert_eq!(response.data["gaps"][1]["kind"], "grammar_parse_timeout");
    assert!(!response.data["text"].as_str().unwrap().ends_with('\n'));
}

isolated!(
    luau_s4_stable_missing_outlines_are_terminal_and_binary_is_excluded,
    missing_outlines
);
fn missing_outlines() {
    use grammar::TestExtraction::*;
    let f = fixture(true);
    write(
        &f.root,
        "plain.ts",
        "export function searchable() {\n return 1;\n}\n",
    );
    write(&f.root, "binary.luau", b"\0\0\0binary");
    let mut overrides = Vec::new();
    for (name, outcome) in [
        ("failed", Failure),
        ("no_outline", NoOutline),
        ("not_luau", MarkersOnly),
    ] {
        let source = format!("local function {name}()\n return 1\nend\n");
        write(&f.root, &format!("{name}.luau"), &source);
        overrides.push(grammar::override_extraction_for_test(
            source.as_bytes(),
            outcome,
        ));
    }
    let checkout = bind(&f.root, &f.dir.join("missing-store"));
    let built = checkout.build().unwrap();
    assert_eq!(built.extract_failures, 1);
    assert!(checkout
        .view()
        .unwrap()
        .current_state()
        .skipped_parses()
        .is_empty());
    let plane = vector_plane(&checkout);
    let filled = plane
        .fill(
            &mut |texts| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
            Default::default(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(filled.files_failed, 3);
    assert_eq!(filled.files_left, 0);
    let result = plane
        .search(&checkout.view().unwrap(), &[1.0, 0.0], 10, &|_| true)
        .unwrap();
    assert_eq!(result.coverage.files, 4);
    assert!(result.coverage.pending.is_empty(), "{:?}", result.coverage);
    assert_eq!(result.coverage.files_failed.len(), 3);
    assert!(result.results.iter().any(|row| row.name == "searchable"));
    assert_eq!(
        crate::data_layer::vectors::LUAU_OUTLINE_FAILED,
        "Meaning search skipped this Luau file because AFT could not extract its outline."
    );
    for (_, reason) in &result.coverage.files_failed {
        assert_eq!(reason, crate::data_layer::vectors::LUAU_OUTLINE_FAILED);
    }
    assert!(!crate::data_layer::meaning::waiting(&result.coverage));
    assert_eq!(
        plane
            .fill(
                &mut |_| panic!("terminal paths must not embed again"),
                Default::default(),
                &mut |_| {}
            )
            .unwrap()
            .files_queued,
        0
    );
}

isolated!(
    luau_s4_coverage_cache_invalidates_on_unpublished_unavailable_gap,
    || coverage_cache(false)
);
isolated!(
    luau_s4_coverage_cache_invalidates_on_unpublished_timeout_gap,
    || coverage_cache(true)
);
fn coverage_cache(timeout: bool) {
    let f = fixture(true);
    write(
        &f.root,
        "plain.ts",
        "export function searchable() {\n return 1;\n}\n",
    );
    let checkout = bind(&f.root, &f.dir.join("cache-store"));
    checkout.build().unwrap();
    write(&f.root, "source.luau", SOURCE);
    checkout.record_paths(&[b"source.luau".to_vec()]).unwrap();
    let before = checkout.view().unwrap();
    let plane = vector_plane(&checkout);
    assert!(plane
        .coverage(&before)
        .unwrap()
        .pending
        .iter()
        .any(|path| path.ends_with("source.luau")));
    assert!(plane.cached_snapshot(&before).is_some());
    let _forced = (!timeout).then(|| {
        f.registry
            .force_unavailable_for_test(crate::grammar::GrammarState::NetworkRetry)
    });
    checkout.refresh_callgraph().unwrap().unwrap();
    let after = checkout.view().unwrap();
    assert_eq!(
        before.text_generation(),
        after.text_generation(),
        "no publication or text change may hide the cache regression"
    );
    assert_ne!(before.parse_gaps_identity(), after.parse_gaps_identity());
    assert!(plane.cached_snapshot(&after).is_none());
    let coverage = plane.coverage(&after).unwrap();
    assert_eq!(coverage.skipped_parses.len(), 1);
    assert!(!coverage
        .pending
        .iter()
        .any(|path| path.ends_with("source.luau")));
    assert_eq!(
        plane
            .cached_snapshot(&after)
            .unwrap()
            .coverage()
            .skipped_parses
            .len(),
        1
    );
}

isolated!(
    luau_s4_trace_to_symbol_discloses_every_early_gap_and_caps_previews,
    trace_gaps
);
fn trace_gaps() {
    let f = fixture(true);
    let origin = write(
        &f.root,
        "origin.luau",
        "local function origin()\n return 0\nend\n",
    );
    let target = write(
        &f.root,
        "target.luau",
        "local function target()\n return 1\nend\n",
    );
    write(
        &f.root,
        "other.luau",
        "local function target()\n return 2\nend\n",
    );
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    let clean = graph_ask(
        &f,
        json!({"op":"trace_to_symbol","path":origin,"symbol":"origin","toSymbol":"target","toPath":target}),
    );
    assert_eq!(
        serde_json::to_string(&clean).unwrap(),
        r#"{"id":"luau-s4","success":true,"path":null,"complete":true,"reason":"no_path_found"}"#
    );
    let _forced = f
        .registry
        .force_unavailable_for_test(crate::grammar::GrammarState::NetworkRetry);
    let paths = (0..6)
        .map(|n| {
            write(
                &f.root,
                &format!("gap-{n}.luau"),
                "local function hidden()\n return 3\nend\n",
            )
        })
        .collect::<Vec<_>>();
    runtime.note_paths(&paths);
    runtime.request_refresh();
    runtime.wait_for_memory_profile_idle_for_test();
    for (params, reason) in [
        (
            json!({"op":"trace_to_symbol","path":origin,"symbol":"origin","toSymbol":"target","toPath":target}),
            "no_path_found",
        ),
        (
            json!({"op":"trace_to_symbol","path":origin,"symbol":"origin","toSymbol":"hidden"}),
            "target_symbol_not_found",
        ),
        (
            json!({"op":"trace_to_symbol","path":origin,"symbol":"origin","toSymbol":"target"}),
            "ambiguous_target",
        ),
    ] {
        let response = graph_ask(&f, params);
        assert_eq!(
            response.data[if response.success { "reason" } else { "code" }],
            reason,
            "{:?}",
            response.data
        );
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["gaps"].as_array().unwrap().len(), 3);
        assert_eq!(response.data["skipped_parses"], 6);
        assert_eq!(response.data["skipped_parses_omitted"], 3);
        assert!(response.data["text"]
            .as_str()
            .unwrap()
            .contains("and 3 more"));
        assert!(!response.data["text"].as_str().unwrap().ends_with('\n'));
    }
    let mut response = Response::success("merge", json!({"gaps":[{"kind":"existing"}]}));
    crate::data_layer::surfaces::disclose_parse_gaps(
        &runtime.view().unwrap(),
        &[f.root.clone()],
        &mut response,
    );
    assert_eq!(response.data["gaps"][0]["kind"], "existing");
    assert_eq!(response.data["gaps"].as_array().unwrap().len(), 4);
}

isolated!(
    luau_s4_ready_before_gap_publication_reruns_an_active_fill,
    || readiness(true)
);
isolated!(
    luau_s4_ready_after_gap_publication_reruns_an_active_fill,
    || readiness(false)
);
fn readiness(before_gap_publication: bool) {
    use test_gates::{hold, Point::*};
    let f = fixture(false);
    let builtin = f.dir.join("builtin");
    write(
        &builtin,
        "plain.ts",
        "export function plain() { return 1; }\n",
    );
    let no_luau = DataCheckout::open(&builtin, &f.storage, None).unwrap();
    no_luau.wait_for_memory_profile_idle_for_test();
    assert!(!no_luau.grammar_driver_running_for_test());
    assert_eq!(f.registry.request_count(), 0);
    // The companion's published gap drives the shared retry when the first
    // checkout is paused with a gap still private to its cold build.
    let companion = before_gap_publication.then(|| {
        let root = f.dir.join("companion");
        write(&root, "companion.luau", SOURCE);
        let runtime = DataCheckout::open(&root, &f.storage, None).unwrap();
        until(
            || {
                runtime
                    .view()
                    .filter(|view| !view.current_state().skipped_parses().is_empty())
            },
            "companion never published its gap",
        );
        assert!(f.registry.wait_until_settled(Duration::from_secs(30)));
        runtime
    });
    write(&f.root, "source.luau", SOURCE);
    let build = before_gap_publication.then(|| hold(&f.root, BuildPublication));
    let fill = hold(&f.root, FillSurvey);
    let publication = hold(&f.root, DeltaPublication);
    let _model = crate::data_layer::meaning::fill_with_constant_vectors_for_test(&f.root, 2);
    let runtime = DataCheckout::open(&f.root, &f.storage, None).unwrap();
    runtime.configure_meaning(Some(&crate::config::SemanticBackendConfig::default()));
    if let Some(build) = &build {
        build.wait_entered();
    } else {
        until(
            || {
                runtime
                    .view()
                    .filter(|view| !view.current_state().skipped_parses().is_empty())
            },
            "checkout never published its grammar gap",
        );
        fill.wait_entered();
        assert!(f.registry.wait_until_settled(Duration::from_secs(30)));
    }
    assert_eq!(f.registry.request_count(), 1);
    write(&f.dir, "transport", &f.wasm);
    write(&f.dir, "clock", "30000");
    // No command or explicit request drives this transition: the checkout's
    // maintenance thread observes the paused clock's newly due retry.
    until(
        || f.registry.status_snapshot().is_none().then_some(()),
        "maintenance never requested the due grammar retry",
    );
    assert_eq!(f.registry.request_count(), 2);
    drop(build);
    fill.wait_entered();
    publication.wait_entered();
    assert!(runtime
        .view()
        .unwrap()
        .outline_symbols(b"source.luau")
        .unwrap()
        .is_none());
    drop(publication);
    until(
        || {
            runtime
                .view()
                .filter(|view| view.outline_symbols(b"source.luau").unwrap().is_some())
        },
        "readiness retry was not published",
    );
    // Release a fill holding the old publication only after the replacement
    // publication has tried to start its own fill while that fill is running.
    drop(fill);
    let snapshot = until(
        || match runtime.meaning_status() {
            crate::data_layer::meaning::MeaningState::Serving { snapshot, .. }
                if snapshot.coverage().pending.is_empty()
                    && snapshot.coverage().files_embedded == 1 =>
            {
                Some(snapshot)
            }
            _ => None,
        },
        "publication did not rerun the already-active fill",
    );
    assert!(snapshot.coverage().skipped_parses.is_empty());
    assert!(snapshot.coverage().files_failed.is_empty());
    assert_eq!(
        std::fs::read_to_string(f.dir.join("clock")).unwrap(),
        "30000"
    );
    drop(companion);
}

isolated!(
    luau_s4_cold_zoom_enrichment_and_single_binding_data_flow_work,
    zoom_and_trace
);
fn zoom_and_trace() {
    let f = fixture(true);
    let path = write(&f.root, "flow.luau", "local function sink(accepted)\n return accepted\nend\nlocal function origin(input)\n local local_copy = input\n plain_copy = local_copy\n sink(plain_copy)\nend\nlocal function multi(input)\n local a, b = input, 2\n sink(a)\nend\nlocal function overwritten(input)\n local a = input\n local b = 2\n a, b = b, a\n sink(a)\nend\n");
    // The zoom request precedes any outline or symbol lookup that could
    // populate the command provider's symbol cache.
    let zoom = ask(
        &f.ctx,
        "zoom",
        json!({"file":path,"symbol":"origin","callgraph":true,"context_lines":0}),
    );
    assert!(zoom.success, "{:?}", zoom.data);
    assert!(zoom.data["content"]
        .as_str()
        .unwrap()
        .contains("local_copy = input"));
    assert!(
        zoom.data["annotations"]["calls_out"]
            .to_string()
            .contains("sink"),
        "{:?}",
        zoom.data
    );
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    let traced = ask(
        &f.ctx,
        "trace_data",
        json!({"file":path,"symbol":"origin","expression":"input","depth":5}),
    );
    assert!(traced.success, "{:?}", traced.data);
    let hops = traced.data["hops"].as_array().unwrap();
    for name in ["local_copy", "plain_copy"] {
        assert!(
            hops.iter()
                .any(|hop| hop["variable"] == name && hop["flow_type"] == "assignment"),
            "{name}: {:?}",
            traced.data
        );
    }
    assert!(
        hops.iter().any(|hop| hop["variable"] == "accepted"
            && hop["flow_type"] == "parameter"
            && hop["symbol"] == "sink"),
        "{:?}",
        traced.data
    );
    let multi = ask(
        &f.ctx,
        "trace_data",
        json!({"file":path,"symbol":"multi","expression":"input"}),
    );
    assert!(multi.success, "{:?}", multi.data);
    assert_eq!(multi.data["hops"], json!([]));
    assert_eq!(multi.data["depth_limited"], false);
    assert!(
        multi.data.get("complete").is_none(),
        "no generic unfollowed-hop flag exists"
    );
    let overwritten = ask(
        &f.ctx,
        "trace_data",
        json!({"file":path,"symbol":"overwritten","expression":"input"}),
    );
    assert!(overwritten.success, "{:?}", overwritten.data);
    assert!(!overwritten.data["hops"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hop| hop["flow_type"] == "parameter"));
}

isolated!(
    luau_s4_outline_size_guard_precedes_timeout_and_nongrammar_errors_stay_skips,
    outline_guards
);
fn outline_guards() {
    use std::io::Write as _;
    let f = fixture(true);
    let large = f.root.join("large.luau");
    let mut file = std::fs::File::create(&large).unwrap();
    let block = "-- comment\n".repeat(4096);
    for _ in 0..1200 {
        file.write_all(block.as_bytes()).unwrap();
    }
    drop(file);
    assert!(std::fs::metadata(&large).unwrap().len() > 50 * 1024 * 1024);
    let bad = write(&f.root, "bad.luau", b"local x = '\xff'\n");
    let files = ask(&f.ctx, "outline", json!({"file":f.root,"files":true}));
    assert!(files.success, "{:?}", files.data);
    assert!(
        files.data.get("gaps").is_none(),
        "size guard must precede a zero-budget parse: {:?}",
        files.data
    );
    let batch = ask(&f.ctx, "outline", json!({"files":[large,bad]}));
    assert!(batch.success, "{:?}", batch.data);
    assert!(batch.data.get("gaps").is_none());
    let skipped = batch.data["skipped_files"].as_array().unwrap();
    assert!(skipped.iter().any(|row| row["reason"] == "too_large"));
    assert!(skipped.iter().any(|row| row["reason"] == "file_not_found"));
}
