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
            assert_eq!(response.data["path"], json!(path));
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
    let _fill = enable_meaning(&f.ctx, &f.root);
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    assert!(runtime
        .view()
        .unwrap()
        .current_state()
        .skipped_parses()
        .is_empty());
    wait_meaning(&f.ctx);
    let response = meaning_query(&f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], false, "{:?}", response.data);
    assert_eq!(
        response.data["semantic_status"], "ready",
        "terminal failures are not a running fill"
    );
    assert_eq!(response.data["semantic_gap"]["pending"], json!([]));
    let failed = response.data["semantic_gap"]["failed_files"]
        .as_array()
        .unwrap();
    assert_eq!(failed.len(), 3, "{:?}", response.data);
    for name in ["failed.luau", "no_outline.luau", "not_luau.luau"] {
        let row = failed.iter().find(|row| row["path"] == name).unwrap();
        assert_eq!(
            row["reason"],
            "Meaning search skipped this Luau file because AFT could not extract its outline."
        );
        assert!(response.data["text"].as_str().unwrap().contains(name));
    }
    assert!(response.data["text"]
        .as_str()
        .unwrap()
        .contains("searchable"));
    assert!(response.data["text"].as_str().unwrap().contains(
        "Meaning search skipped this Luau file because AFT could not extract its outline."
    ));
    assert!(!response.data["semantic_gap"]
        .to_string()
        .contains("binary.luau"));
    let coverage = crate::data_layer::tools::search::DataSemantic::current(&f.ctx)
        .unwrap()
        .serving()
        .unwrap()
        .coverage()
        .clone();
    assert_eq!(coverage.files, 4);
    assert!(!crate::data_layer::meaning::waiting(&coverage));
    assert!(!coverage.complete());
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
            .contains("shown 3 of 6 files (cap) · narrow: path"));
        assert!(!response.data["text"]
            .as_str()
            .unwrap()
            .contains("and 3 more"));
        assert_eq!(response.data["parse_gaps_list"]["shown"], 3);
        assert_eq!(response.data["parse_gaps_list"]["total"]["value"], 6);
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
    luau_s4_outline_size_guard_and_syntax_errors_are_truthful_skips,
    outline_guards
);
fn outline_guards() {
    let f = fixture(true);
    let large = large_luau(&f.root);
    let bad = write(
        &f.root,
        "nested/bad.luau",
        "local function broken()\n return @@@\nend\n",
    );
    let bad_source = std::fs::read_to_string(&bad).unwrap();
    let bad_tree = crate::parser::parse_luau_source(&bad, &bad_source).unwrap();
    assert!(
        bad_tree.root_node().has_error(),
        "fixture must reach a real Tree-sitter syntax error: {}",
        bad_tree.root_node().to_sexp()
    );
    let files = ask(&f.ctx, "outline", json!({"file":f.root,"files":true}));
    assert!(files.success, "{:?}", files.data);
    assert_eq!(files.data["complete"], false, "{:?}", files.data);
    assert!(files.data.get("gaps").is_none());
    let skipped = files.data["skipped_files"].as_array().unwrap();
    assert!(skipped
        .iter()
        .any(|row| row["file"] == "large.luau" && row["reason"] == "too_large"));
    assert!(
        skipped
            .iter()
            .any(|row| row["file"] == "nested/bad.luau" && row["reason"] == "parse_error"),
        "{:?}",
        files.data
    );
    let batch = ask(&f.ctx, "outline", json!({"files":[large,bad]}));
    assert!(batch.success, "{:?}", batch.data);
    assert_eq!(batch.data["complete"], false);
    assert!(batch.data.get("gaps").is_none());
    let skipped = batch.data["skipped_files"].as_array().unwrap();
    assert!(skipped.iter().any(|row| row["reason"] == "too_large"));
    assert!(skipped.iter().any(|row| row["reason"] == "parse_error"));
    let directory = ask(&f.ctx, "outline", json!({"directory":f.root}));
    assert!(directory.success, "{:?}", directory.data);
    assert_eq!(directory.data["complete"], false, "{:?}", directory.data);
}

fn large_luau(root: &Path) -> PathBuf {
    use std::io::Write as _;
    std::fs::create_dir_all(root).unwrap();
    let large = root.join("large.luau");
    let mut file = std::fs::File::create(&large).unwrap();
    let block = "-- comment\n".repeat(4096);
    for _ in 0..1200 {
        file.write_all(block.as_bytes()).unwrap();
    }
    assert!(file.metadata().unwrap().len() > 50 * 1024 * 1024);
    large
}

isolated!(
    luau_s4_rolled_up_size_guard_precedes_a_zero_budget_timeout,
    || {
        let f = fixture(true);
        large_luau(&f.root.join("nested"));
        let response = ask(&f.ctx, "outline", json!({"file":f.root,"files":true}));
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["complete"], false);
        assert!(
            response.data.get("gaps").is_none(),
            "the 50 MiB guard must run before the zero-budget parser: {:?}",
            response.data
        );
        assert!(response.data["skipped_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["file"] == "nested/large.luau" && gap["reason"] == "too_large"));
    }
);

isolated!(
    luau_s4_meaning_transient_gaps_name_files_and_use_standard_preview,
    meaning_transient
);
fn meaning_transient() {
    use crate::grammar::GrammarState::*;
    let f = fixture(true);
    let _hold = f.registry.force_unavailable_for_test(NotDownloaded);
    for n in 0..6 {
        write(&f.root, &format!("file-{n}.luau"), SOURCE);
    }
    write(
        &f.root,
        "plain.ts",
        "export function searchable() { return 1; }\n",
    );
    let _fill = enable_meaning(&f.ctx, &f.root);
    f.ctx
        .data_checkout()
        .unwrap()
        .wait_for_memory_profile_idle_for_test();
    wait_meaning(&f.ctx);
    for state in [
        NotDownloaded,
        Downloading,
        NetworkRetry,
        StorageRetry,
        AttemptsExhausted,
    ] {
        let _state = f.registry.force_unavailable_for_test(state);
        let response = meaning_query(&f.ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["complete"], false, "{:?}", response.data);
        assert_eq!(
            response.data["semantic_status"],
            if state == AttemptsExhausted {
                "ready"
            } else {
                "refreshing"
            },
            "{:?}",
            response.data
        );
        let gap = &response.data["semantic_gap"];
        assert_eq!(gap["pending"], json!([]));
        assert_eq!(gap["skipped_files_count"], 6);
        assert_eq!(gap["skipped_files_omitted"], 3);
        assert_eq!(gap["skipped_files"].as_array().unwrap().len(), 3);
        let text = response.data["text"].as_str().unwrap();
        for n in 0..3 {
            assert!(text.contains(&format!("file-{n}.luau")), "{text}");
        }
        assert!(
            text.contains("shown 3 of 6 files (cap) · narrow: path"),
            "{text}"
        );
        assert!(!text.contains("and 3 more"));
        assert!(!text.ends_with("\n\n"));
    }
}

isolated!(
    luau_s4_files_rollup_discloses_unavailable_luau_members,
    outline_rollup
);
fn outline_rollup() {
    let f = fixture(true);
    let _state = f
        .registry
        .force_unavailable_for_test(crate::grammar::GrammarState::NetworkRetry);
    let path = write(&f.root, "nested/deeper/source.luau", SOURCE);
    write(
        &f.root,
        "plain.ts",
        "export function searchable() { return 1; }\n",
    );
    let response = ask(&f.ctx, "outline", json!({"file":f.root,"files":true}));
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], false, "{:?}", response.data);
    assert_eq!(response.data["gaps"][0]["path"], json!(path));
    assert_eq!(response.data["gaps"][0]["state"], "network_retry");
    assert!(response.data["text"]
        .as_str()
        .unwrap()
        .contains("The Luau grammar"));
    assert!(response.data["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "nested/deeper/source.luau" && file.get("symbols").is_none()));
}

fn inspect_category(ctx: &AppContext, scope: &Path, category: &str) -> Response {
    until(
        || {
            let params = if category == "dead_code" {
                json!({"sections":[category]})
            } else {
                json!({"scope":[scope],"sections":[category]})
            };
            let response = ask(ctx, "inspect", params);
            let deadline = response.data["code"] == "inspect_not_fresh"
                && response.data["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("deadline elapsed"));
            (!deadline).then_some(response)
        },
        "inspect category missed its deadline",
    )
}

fn configure_inspect(f: &Fixture) {
    configure_inspect_with_graph(f, true);
}

fn configure_inspect_with_graph(f: &Fixture, callgraph: bool) {
    let request: RawRequest = serde_json::from_value(json!({
        "id":"luau-s4-configure", "command":"configure", "harness":"opencode",
        "project_root":f.root, "storage_dir":f.storage,
        "config":[{"tier":"user","source":f.dir.join("aft.jsonc"),"doc":json!({
            "indexes":{"trigram":true,"semantic":false,"callgraph":callgraph},
            "inspect":{"categories":{"diagnostics":false}}
        }).to_string()}]
    }))
    .unwrap();
    let response = crate::commands::configure::handle_configure(&request, &f.ctx);
    assert!(response.success, "{:?}", response.data);
}

isolated!(
    luau_s4_inspect_todos_terminal_gaps_are_final_not_pending,
    || todo_gaps(true)
);
isolated!(
    luau_s4_inspect_todos_transient_gaps_retry_to_counted,
    || todo_gaps(false)
);
fn todo_gaps(terminal: bool) {
    use crate::grammar::GrammarState;
    let f = fixture(terminal);
    write(
        &f.root,
        "plain.ts",
        "// TODO: builtin marker\nexport function searchable() { return 1; }\n",
    );
    write(
        &f.root,
        "source.luau",
        format!("-- TODO: dynamic marker\n{SOURCE}"),
    );
    let _forced = terminal.then(|| {
        f.registry
            .force_unavailable_for_test(GrammarState::AttemptsExhausted)
    });
    configure_inspect(&f);
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    assert_eq!(
        runtime
            .view()
            .unwrap()
            .current_state()
            .skipped_parses()
            .len(),
        1
    );
    // The second request checks reuse of stored TODO counts without waiting
    // for an edit or new publication.
    for _ in 0..2 {
        let started = Instant::now();
        let response = inspect_category(&f.ctx, &f.root, "todos");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "TODO query waited for a nonexistent record"
        );
        assert!(response.success, "{:?}", response.data);
        let todos = &response.data["summary"]["todos"];
        assert_ne!(todos["status"], "pending", "{:?}", response.data);
        assert_eq!(todos["complete"], false, "{:?}", response.data);
        assert_eq!(todos["count"], 1, "{:?}", response.data);
        assert!(todos["skipped_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["file"] == "source.luau"
                && gap["reason"] == "no TODO record for this file in the index store"));
    }
    if !terminal {
        assert!(f.registry.wait_until_settled(Duration::from_secs(30)));
        assert_eq!(f.registry.request_count(), 1);
        write(&f.dir, "transport", &f.wasm);
        write(&f.dir, "clock", "30000");
        until(
            || {
                runtime.view().filter(|view| {
                    view.current_state().skipped_parses().is_empty()
                        && !runtime.checkout().has_unpublished_luau_changes()
                })
            },
            "grammar-ready TODO gap did not publish",
        );
        let response = inspect_category(&f.ctx, &f.root, "todos");
        assert!(response.success, "{:?}", response.data);
        let todos = &response.data["summary"]["todos"];
        assert_eq!(todos["count"], 2, "{:?}", response.data);
        assert_ne!(todos["complete"], false, "{:?}", response.data);
    }
}

isolated!(
    luau_s4_round4_inspect_walk_admits_exported_luau,
    dead_code_gaps
);
fn dead_code_gaps() {
    let mut f = fixture(true);
    for mixed in [false, true] {
        f.root = f.dir.join(if mixed { "mixed" } else { "luau-only" });
        f.ctx = context(&f.root, &f.storage);
        write(
            &f.root,
            "source.luau",
            "function exported(input)\n return input + 1\nend\n",
        );
        if mixed {
            write(
                &f.root,
                "plain.ts",
                "export function searchable() { return 1; }\n",
            );
        }
        let _build = crate::data_layer::runtime::hold_builds_for_test(&f.root);
        configure_inspect_with_graph(&f, false);
        assert!(f.ctx.data_checkout().unwrap().view().is_none());
        let response = inspect_category(&f.ctx, &f.root, "dead_code");
        assert!(response.success, "{:?}", response.data);
        let dead = &response.data["summary"]["dead_code"];
        assert_eq!(dead["complete"], false, "{:?}", response.data);
        if !mixed {
            assert!(dead["count"].is_null(), "{:?}", response.data);
        }
        assert!(dead["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "language_unsupported"
                && gap["language"] == "luau"
                && gap["files"] == 1));
        assert!(response.data["text"].as_str().unwrap().contains("luau"));
        assert!(!dead["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["file"] == "source.luau")));
        assert!(f.ctx.data_checkout().unwrap().view().is_none());
    }
}

isolated!(
    luau_s4_failed_readiness_publication_retries_without_an_edit_or_graph_query,
    publication_retry
);
fn publication_retry() {
    let f = fixture(false);
    write(&f.root, "source.luau", SOURCE);
    write(
        &f.root,
        "plain.ts",
        "export function searchable() { return 1; }\n",
    );
    let _fill = enable_meaning(&f.ctx, &f.root);
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    wait_meaning(&f.ctx);
    assert!(f.registry.wait_until_settled(Duration::from_secs(30)));
    assert_eq!(f.registry.request_count(), 1);
    let publication = test_gates::hold(&f.root, test_gates::Point::DeltaPublication);
    write(&f.dir, "transport", &f.wasm);
    write(&f.dir, "clock", "30000");
    publication.wait_entered();
    assert_eq!(f.registry.request_count(), 2);
    let low = crate::disk_floor::set_free_space_for_test(&f.storage, 1 << 30, 1 << 40);
    drop(publication);
    runtime.wait_for_memory_profile_idle_for_test();
    assert!(runtime.checkout().has_unpublished_luau_changes());
    let response = meaning_query(&f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], false, "{:?}", response.data);
    assert!(response.data["semantic_gap"]["pending"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path == "source.luau"));
    drop(low);
    // Observe only: neither view() nor has_unpublished_luau_changes() starts
    // refresh work. The failed publication must have armed its own retry.
    until(
        || {
            runtime.view().filter(|view| {
                view.current_state().skipped_parses().is_empty()
                    && !runtime.checkout().has_unpublished_luau_changes()
            })
        },
        "failed Luau publication had no retry driver",
    );
    wait_meaning(&f.ctx);
    let response = meaning_query(&f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["complete"], true, "{:?}", response.data);
    assert!(response.data.get("semantic_gap").is_none());
}

isolated!(
    luau_s4_round4_terminal_only_search_serves_approved_gaps,
    || {
        let f = fixture(true);
        let mut overrides = Vec::new();
        for n in 0..6 {
            let source = format!("function failed_{n}()\n return {n}\nend\n");
            write(&f.root, &format!("failed-{n}.luau"), &source);
            overrides.push(grammar::override_extraction_for_test(
                source.as_bytes(),
                grammar::TestExtraction::NoOutline,
            ));
        }
        let _fill = enable_meaning(&f.ctx, &f.root);
        let runtime = f.ctx.data_checkout().unwrap();
        runtime.wait_for_memory_profile_idle_for_test();
        let _ = meaning_query(&f.ctx);
        until(
            || match runtime.meaning_status() {
                crate::data_layer::meaning::MeaningState::Serving { snapshot, .. }
                    if snapshot.coverage().pending.is_empty() =>
                {
                    Some(())
                }
                _ => None,
            },
            "terminal-only fill did not settle",
        );
        let response = meaning_query(&f.ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(
            response.data["semantic_status"], "ready",
            "{:?}",
            response.data
        );
        assert_eq!(response.data["complete"], false, "{:?}", response.data);
        let gap = &response.data["semantic_gap"];
        assert_eq!(gap["pending"], json!([]));
        assert_eq!(gap["failed_files"].as_array().unwrap().len(), 3);
        assert_eq!(gap["failed_files_count"], 6);
        assert_eq!(gap["failed_files_omitted"], 3);
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains(
                "Meaning search skipped this Luau file because AFT could not extract its outline."
            ),
            "{text}"
        );
        assert!(
            text.contains("shown 3 of 6 files (cap) · narrow: path"),
            "{text}"
        );
    }
);

isolated!(luau_s4_round4_todos_before_delta_publication, || {
    todos_during_publication(false)
});
isolated!(luau_s4_round4_todos_after_failed_delta_publication, || {
    todos_during_publication(true)
});
fn todos_during_publication(deferred: bool) {
    let f = fixture(false);
    write(
        &f.root,
        "source.luau",
        format!("-- TODO: ready marker\n{SOURCE}"),
    );
    configure_inspect(&f);
    let runtime = f.ctx.data_checkout().unwrap();
    runtime.wait_for_memory_profile_idle_for_test();
    assert!(f.registry.wait_until_settled(Duration::from_secs(30)));
    let publication = test_gates::hold(&f.root, test_gates::Point::DeltaPublication);
    write(&f.dir, "transport", &f.wasm);
    write(&f.dir, "clock", "30000");
    publication.wait_entered();
    let publication = if deferred {
        let low = crate::disk_floor::set_free_space_for_test(&f.storage, 1 << 30, 1 << 40);
        drop(publication);
        runtime.wait_for_memory_profile_idle_for_test();
        (None, Some(low))
    } else {
        (Some(publication), None)
    };
    let view = runtime.view().unwrap();
    assert!(view.current_state().skipped_parses().is_empty());
    assert!(runtime.checkout().has_unpublished_luau_changes());
    for _ in 0..2 {
        let start = Instant::now();
        let response = inspect_category(&f.ctx, &f.root, "todos");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "TODO query waited for delta publication: {:?}",
            response.data
        );
        assert!(response.success, "{:?}", response.data);
        assert_eq!(
            response.data["summary"]["todos"]["count"], 1,
            "{:?}",
            response.data
        );
        assert_ne!(response.data["summary"]["todos"]["complete"], false);
        assert!(response.data["text"]
            .as_str()
            .unwrap()
            .contains("ready marker"));
    }
    drop(publication);
}

isolated!(
    luau_s4_round4_todo_gap_invalidates_a_previously_successful_memo,
    || {
        let f = fixture(true);
        let original = format!("-- TODO: original marker\n{SOURCE}");
        let path = write(&f.root, "source.luau", &original);
        configure_inspect(&f);
        let runtime = f.ctx.data_checkout().unwrap();
        runtime.wait_for_memory_profile_idle_for_test();
        let first = inspect_category(&f.ctx, &f.root, "todos");
        assert_eq!(
            first.data["summary"]["todos"]["count"], 1,
            "{:?}",
            first.data
        );
        let _forced = f
            .registry
            .force_unavailable_for_test(crate::grammar::GrammarState::AttemptsExhausted);
        for source in [format!("{original}-- changed\n"), original] {
            write(&f.root, "source.luau", source);
            runtime.note_paths(&[path.clone()]);
            runtime.request_refresh();
            runtime.wait_for_memory_profile_idle_for_test();
        }
        assert_eq!(
            runtime
                .view()
                .unwrap()
                .current_state()
                .skipped_parses()
                .len(),
            1
        );
        let response = inspect_category(&f.ctx, &f.root, "todos");
        assert_eq!(
            response.data["summary"]["todos"]["count"], 0,
            "{:?}",
            response.data
        );
        assert_eq!(response.data["summary"]["todos"]["complete"], false);
    }
);

isolated!(
    luau_s4_round4_outline_cached_counts_do_not_parse_and_manifest_excludes_skips,
    || {
        let f = fixture(true);
        let warm = write(&f.root, "warm.luau", SOURCE);
        crate::parser::list_symbols_shared(f.ctx.provider(), &warm, true).unwrap();
        for n in 0..512 {
            write(
                &f.root,
                &format!("nested/{n:04}-{}-cold.luau", "member".repeat(16)),
                SOURCE,
            );
        }
        large_luau(&f.root);
        let _forced = f
            .registry
            .force_unavailable_for_test(crate::grammar::GrammarState::NetworkRetry);
        let before = crate::parser::work_counters::tree_parses();
        let response = ask(&f.ctx, "outline", json!({"file":f.root,"files":true}));
        assert_eq!(crate::parser::work_counters::tree_parses(), before);
        assert!(response.success, "{:?}", response.data);
        assert!(
            response.data["rollup_count"].as_u64().unwrap() > 0,
            "fixture must roll up the nested files"
        );
        let warm_row = response.data["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == "warm.luau")
            .unwrap();
        assert_eq!(warm_row["symbols"], 1, "{:?}", response.data);
        let manifest = response.data["content_files"].as_array().unwrap();
        assert!(
            !manifest.iter().any(|path| path
                .as_str()
                .is_some_and(|path| path.ends_with("cold.luau") || path.ends_with("large.luau"))),
            "{:?}",
            response.data
        );
    }
);

enum StructureError {
    Syntax,
    Timeout,
}
struct NoBatch(Option<StructureError>);
impl crate::language::LanguageProvider for NoBatch {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn resolve_symbol(
        &self,
        file: &Path,
        name: &str,
    ) -> Result<Vec<crate::symbols::SymbolMatch>, crate::error::AftError> {
        crate::language::LanguageProvider::resolve_symbol(
            &crate::parser::TreeSitterProvider::new(),
            file,
            name,
        )
    }
    fn list_symbols(
        &self,
        file: &Path,
    ) -> Result<Vec<crate::symbols::Symbol>, crate::error::AftError> {
        match self.0 {
            Some(StructureError::Syntax) => {
                return Err(crate::error::AftError::ParseError {
                    message: "injected structure failure".into(),
                })
            }
            Some(StructureError::Timeout) => {
                return Err(crate::error::AftError::GrammarParseTimeout {
                    path: file.display().to_string(),
                    language: "luau".into(),
                    timeout_ms: 30_000,
                })
            }
            None => (),
        }
        crate::parser::FileParser::new()
            .with_dynamic_languages(true)
            .extract_symbols(file)
    }
}
isolated!(
    luau_s4_round4_outline_syntax_only_in_batch_and_fallback_directories,
    || {
        let f = fixture(true);
        write(&f.root, "bad.luau", "function broken()\n return @@@\nend\n");
        let fallback = AppContext::new(Box::new(NoBatch(None)), (*f.ctx.config()).clone());
        fallback.set_canonical_cache_root(f.root.clone());
        for ctx in [&fallback, &f.ctx] {
            for params in [
                json!({"directory":f.root}),
                json!({"file":f.root,"files":true}),
            ] {
                let response = ask(ctx, "outline", params);
                assert!(response.success, "{:?}", response.data);
                assert_eq!(response.data["complete"], false, "{:?}", response.data);
                assert!(response.data["skipped_files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|gap| gap["file"] == "bad.luau" && gap["reason"] == "parse_error"));
                assert!(response.data.get("gaps").is_none());
            }
        }
    }
);

isolated!(
    luau_s4_round4_outline_late_structure_errors_are_incomplete_and_not_read,
    || {
        let f = fixture(true);
        let path = write(&f.root, "source.luau", SOURCE);
        for error in [StructureError::Syntax, StructureError::Timeout] {
            let timeout = matches!(error, StructureError::Timeout);
            let ctx = AppContext::new(Box::new(NoBatch(Some(error))), (*f.ctx.config()).clone());
            ctx.set_canonical_cache_root(f.root.clone());
            let response = ask(&ctx, "outline", json!({"directory":f.root}));
            assert!(response.success, "{:?}", response.data);
            assert_eq!(response.data["complete"], false, "{:?}", response.data);
            assert_eq!(response.data["content_files"], json!([]));
            if timeout {
                assert_eq!(response.data["gaps"][0]["code"], "grammar_parse_timeout");
                assert_eq!(response.data["gaps"][0]["path"], json!(path));
            } else {
                assert_eq!(response.data["skipped_files"][0]["reason"], "parse_error");
                assert!(response.data.get("gaps").is_none());
            }
        }
    }
);

isolated!(
    luau_s4_round4_unsupported_assignment_preserves_rhs_parameter_hops,
    || {
        let f = fixture(true);
        let path = write(&f.root, "rhs.luau", "local function sink(accepted)\n return accepted\nend\nfunction field(input)\n self.x = sink(input)\nend\nfunction indexed(input)\n t[k] = sink(input)\nend\nfunction parallel(input)\n input, b = sink(input), 2\nend\n");
        f.ctx
            .data_checkout()
            .unwrap()
            .wait_for_memory_profile_idle_for_test();
        for symbol in ["field", "indexed", "parallel"] {
            let response = ask(
                &f.ctx,
                "trace_data",
                json!({"file":path,"symbol":symbol,"expression":"input"}),
            );
            assert!(response.success, "{:?}", response.data);
            assert!(
                response.data["hops"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|hop| hop["flow_type"] == "parameter"
                        && hop["variable"] == "accepted"
                        && hop["symbol"] == "sink"),
                "{symbol}: {:?}",
                response.data
            );
        }
    }
);

isolated!(
    luau_s4_round4_ready_graph_wrappers_and_persisted_gaps,
    || {
        let f = fixture(true);
        let path = write(&f.root, "source.luau", SOURCE);
        let runtime = f.ctx.data_checkout().unwrap();
        runtime.wait_for_memory_profile_idle_for_test();
        let forced = f
            .registry
            .force_unavailable_for_test(crate::grammar::GrammarState::AttemptsExhausted);
        let gap = write(&f.root, "gap.luau", "function hidden()\n return 1\nend\n");
        runtime.note_paths(&[gap.clone()]);
        runtime.request_refresh();
        runtime.wait_for_memory_profile_idle_for_test();
        let _retry = test_gates::hold(&f.root, test_gates::Point::GrammarRetryRead);
        drop(forced);
        for command in ["impact", "call_tree", "trace_to"] {
            let response = ask(&f.ctx, command, json!({"file":path,"symbol":"source"}));
            assert!(response.success, "{command}: {:?}", response.data);
            assert_eq!(
                response.data["complete"], false,
                "{command}: {:?}",
                response.data
            );
            assert!(
                response.data["gaps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["path"] == json!(gap)
                        && entry["state"] == "attempts_exhausted")
            );
        }
    }
);

isolated!(
    luau_s4_round4_already_ready_subscription_retries_a_failed_reread,
    || {
        let f = fixture(true);
        write(&f.root, "source.luau", SOURCE);
        let forced = f
            .registry
            .force_unavailable_for_test(crate::grammar::GrammarState::NetworkRetry);
        let build = test_gates::hold(&f.root, test_gates::Point::BuildPublication);
        let subscription = test_gates::hold(&f.root, test_gates::Point::GrammarSubscription);
        let runtime = f.ctx.data_checkout().unwrap();
        build.wait_entered();
        let failed = test_gates::fail_next_grammar_retry(&f.root);
        drop(build);
        subscription.wait_entered();
        drop(forced);
        drop(subscription);
        until(
            || {
                failed
                    .load(std::sync::atomic::Ordering::Acquire)
                    .then_some(())
            },
            "first reread did not reach injected failure",
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if runtime.view().is_some_and(|view| {
                view.current_state().skipped_parses().is_empty()
                    && !runtime.checkout().has_unpublished_luau_changes()
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "already-ready subscription lost its retry driver"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
);
