use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use super::test_seams::*;
use super::*;
use crate::grammar::PRODUCTION_MANIFEST;

const WASM: &[u8] = include_bytes!("../../tests/fixtures/grammars/56b2a562244e6f35f7b320bce59099126185389df1277a93e450ba4156c0e039.wasm");

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl FakeClock {
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

enum Hop {
    Error,
    Response(u16, Option<String>, Vec<u8>),
}

struct Script {
    hops: Mutex<VecDeque<Hop>>,
    calls: AtomicUsize,
    urls: Mutex<Vec<String>>,
}
impl Script {
    fn new(hops: impl IntoIterator<Item = Hop>) -> Arc<Self> {
        Arc::new(Self {
            hops: Mutex::new(hops.into_iter().collect()),
            calls: AtomicUsize::new(0),
            urls: Mutex::new(Vec::new()),
        })
    }
}
impl Transport for Script {
    fn get(&self, url: &reqwest::Url, remaining: Duration) -> Result<Response, DownloadFailure> {
        assert!(remaining <= download::attempt_deadline(WASM.len() as u64));
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.urls.lock().push(url.as_str().into());
        match self
            .hops
            .lock()
            .pop_front()
            .expect("unexpected download attempt")
        {
            Hop::Error => Err(DownloadFailure::Network),
            Hop::Response(status, location, body) => Ok(Response {
                status,
                location,
                body: Box::new(Cursor::new(body)),
            }),
        }
    }
}

fn success() -> Hop {
    Hop::Response(200, None, WASM.to_vec())
}

fn setup(
    hops: impl IntoIterator<Item = Hop>,
) -> (
    tempfile::TempDir,
    Arc<GrammarRegistry>,
    Arc<FakeClock>,
    Arc<Script>,
) {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::default());
    let transport = Script::new(hops);
    let registry = GrammarRegistry::with_test_options(TestOptions {
        manifest: PRODUCTION_MANIFEST.clone(),
        storage_root: directory.path().into(),
        clock: clock.clone(),
        transport: transport.clone(),
        queries: vec!["(chunk) @root".into()],
        fault: StorageFault::None,
    });
    (directory, registry, clock, transport)
}

fn stored(directory: &std::path::Path) {
    crate::private_storage::create_dir_all(directory.join("grammars")).unwrap();
    crate::private_storage::write(stored_path(directory, &PRODUCTION_MANIFEST), WASM).unwrap();
}

fn settled(registry: &GrammarRegistry) {
    assert!(registry.wait_until_settled(Duration::from_secs(30)));
}

#[test]
fn grammar_state_messages_are_pinned() {
    for (state, expected) in [
        (GrammarState::NotDownloaded, "The luau grammar isn't downloaded yet; AFT downloads it on first use."),
        (GrammarState::Downloading, "The luau grammar is downloading; retry in a few seconds."),
        (GrammarState::NetworkRetry, "The luau grammar download failed; AFT tries again after a short wait."),
        (GrammarState::StorageRetry, "AFT can't save the luau grammar right now; it tries again after a short wait."),
        (GrammarState::AttemptsExhausted, "The luau grammar couldn't be downloaded after five attempts; restarting AFT tries again."),
        (GrammarState::VerificationFailed, "The downloaded luau grammar didn't match its expected hash, so AFT didn't use it; restarting AFT tries again."),
        (GrammarState::LoadFailed, "The luau grammar failed to load; restarting AFT tries again."),
        (GrammarState::UnsupportedPlatform, "luau files aren't supported on this platform yet."),
    ] {
        assert_eq!(GrammarUnavailable::new("luau", state).message, expected);
    }
}

#[test]
fn grammar_unavailable_fields_and_wire_code() {
    let (_directory, registry, clock, _) = setup([]);
    let mut entry = registry.entry.lock();
    for state in [
        GrammarState::NotDownloaded,
        GrammarState::Downloading,
        GrammarState::NetworkRetry,
        GrammarState::StorageRetry,
        GrammarState::AttemptsExhausted,
        GrammarState::VerificationFailed,
        GrammarState::LoadFailed,
        GrammarState::UnsupportedPlatform,
    ] {
        entry.state = state;
        entry.cause = Some(FailureCause::Storage);
        entry.retry_at = 100;
        clock.0.store(25, Ordering::SeqCst);
        let unavailable = registry.unavailable(&entry);
        let json = crate::error::AftError::from(unavailable.clone()).to_error_json();
        assert_eq!(json["code"], "grammar_unavailable");
        assert_eq!(json["language"], "luau");
        assert_eq!(json["state"], serde_json::to_value(state).unwrap());
        assert_eq!(
            json.get("cause").is_some(),
            state == GrammarState::AttemptsExhausted
        );
        let retry = matches!(
            state,
            GrammarState::NetworkRetry | GrammarState::StorageRetry
        );
        assert_eq!(json.get("retry_after_ms").is_some(), retry);
        if retry {
            assert_eq!(json["retry_after_ms"], 75);
        }
        clock.advance(100);
        if retry {
            assert_eq!(registry.unavailable(&entry).retry_after_ms, Some(0));
        }
    }
}

#[test]
fn grammar_detection_golden_builtins_and_opt_in() {
    use crate::parser::{detect_language, detect_language_any, LangId::*};
    let mappings = [
        ("ts mts cts", TypeScript),
        ("tsx", Tsx),
        ("js jsx mjs cjs", JavaScript),
        ("py pyi", Python),
        ("rs", Rust),
        ("go", Go),
        ("c h", C),
        ("cc cpp cxx hpp hh", Cpp),
        ("cu cuh", Cuda),
        ("metal", Metal),
        ("zig", Zig),
        ("cs", CSharp),
        ("sh bash zsh", Bash),
        ("html htm", Html),
        ("md markdown mdx qmd Qmd rmd Rmd", Markdown),
        ("sol", Solidity),
        ("scss", Scss),
        ("vue", Vue),
        ("json jsonc", Json),
        ("scala sc", Scala),
        ("java", Java),
        ("rb", Ruby),
        ("kt kts", Kotlin),
        ("swift", Swift),
        ("inc php", Php),
        ("lua", Lua),
        ("pl pm t", Perl),
        ("yaml yml", Yaml),
        ("pas pp dpr dpk lpr", Pascal),
        ("R r", R),
        ("groovy gvy gy gsh gradle", Groovy),
        ("m mm", ObjC),
        ("toml", Toml),
    ];
    for (extensions, language) in mappings {
        for extension in extensions.split_whitespace() {
            let name = format!("nonexistent.{extension}");
            let path = std::path::Path::new(&name);
            assert_eq!(detect_language(path), Some(language), "{extension}");
            assert_eq!(detect_language_any(path, false), Some(language));
            assert_eq!(detect_language_any(path, true), Some(language));
        }
    }
    assert_eq!(
        detect_language(std::path::Path::new("Jenkinsfile")),
        Some(Groovy)
    );
    for extension in ["luau", "LUAU", "unknown"] {
        let name = format!("f.{extension}");
        let path = std::path::Path::new(&name);
        assert_eq!(detect_language(path), None);
        assert_eq!(detect_language_any(path, false), None);
        assert_eq!(
            detect_language_any(path, true),
            (extension == "luau").then_some(Luau)
        );
    }
}

#[test]
fn grammar_stored_first_single_compile_rayon_and_lifetime() {
    let (directory, registry, _, transport) = setup([]);
    stored(directory.path());
    let ready = registry.subscribe();
    let trees = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .unwrap()
        .install(|| {
            use rayon::prelude::*;
            (0..32)
                .into_par_iter()
                .map(|_| {
                    let language = registry.request().unwrap();
                    let mut parser = tree_sitter::Parser::new();
                    prepare_parser(&mut parser).unwrap();
                    parser.set_language(&language).unwrap();
                    let tree = parser
                        .parse("local function f() return 1 end", None)
                        .unwrap();
                    assert!(!tree.root_node().has_error());
                    tree.root_node().to_sexp()
                })
                .collect::<Vec<_>>()
        });
    assert!(trees.iter().all(|tree| tree == &trees[0]));
    assert_eq!(registry.compile_count(), 1);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(1)).unwrap(),
        registry.identity()
    );
    std::fs::remove_file(stored_path(directory.path(), &PRODUCTION_MANIFEST)).unwrap();
    let language = registry.request().unwrap();
    let mut parser = tree_sitter::Parser::new();
    prepare_parser(&mut parser).unwrap();
    parser.set_language(&language).unwrap();
    let after_delete = parser
        .parse("local function f() return 1 end", None)
        .unwrap();
    assert_eq!(after_delete.root_node().to_sexp(), trees[0]);
    assert_eq!(registry.compile_count(), 1);
    assert!(std::fs::read_dir(directory.path().join("grammars"))
        .unwrap()
        .next()
        .is_none());
    eprintln!("Luau cold compile: {:.3} ms", registry.compile_ms());
}

#[test]
fn grammar_backoff_shared_failures_and_no_sixth_attempt() {
    let (_directory, registry, clock, transport) =
        setup([Hop::Error, success(), Hop::Error, success(), Hop::Error]);
    for attempt in 1..=5 {
        if attempt % 2 == 0 {
            registry.set_storage_fault(StorageFault::Write);
        }
        assert_eq!(
            registry.request().unwrap_err().state,
            GrammarState::Downloading
        );
        settled(&registry);
        let error = registry.request().unwrap_err();
        assert_eq!(transport.calls.load(Ordering::SeqCst), attempt);
        assert_eq!(registry.compile_count(), 0);
        if attempt == 5 {
            assert_eq!(error.state, GrammarState::AttemptsExhausted);
            assert_eq!(error.cause, Some(FailureCause::Network));
            assert_eq!(error.retry_after_ms, None);
        } else {
            let backoff = 30_000 << (attempt - 1);
            assert_eq!(
                error.state,
                if attempt % 2 == 0 {
                    GrammarState::StorageRetry
                } else {
                    GrammarState::NetworkRetry
                }
            );
            assert_eq!(error.retry_after_ms, Some(backoff));
            clock.advance(backoff - 1);
            assert_eq!(registry.request().unwrap_err().retry_after_ms, Some(1));
            clock.advance(1);
        }
    }
    clock.advance(u64::MAX / 2);
    assert_eq!(
        registry.request().unwrap_err().state,
        GrammarState::AttemptsExhausted
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 5);
}

#[test]
fn grammar_download_redirect_status_and_verification_table() {
    let cases = vec![
        (vec![Hop::Error], GrammarState::NetworkRetry),
        (
            vec![Hop::Response(503, None, vec![])],
            GrammarState::NetworkRetry,
        ),
        (
            vec![Hop::Response(
                302,
                Some("http://insecure.example/g.wasm".into()),
                vec![],
            )],
            GrammarState::NetworkRetry,
        ),
        (
            vec![Hop::Response(302, None, vec![])],
            GrammarState::NetworkRetry,
        ),
        (
            (0..6)
                .map(|_| Hop::Response(302, Some("/next".into()), vec![]))
                .collect(),
            GrammarState::NetworkRetry,
        ),
        (
            vec![Hop::Response(200, None, vec![0; WASM.len() + 1])],
            GrammarState::VerificationFailed,
        ),
        (
            vec![Hop::Response(200, None, WASM[..WASM.len() - 1].to_vec())],
            GrammarState::VerificationFailed,
        ),
        (
            vec![Hop::Response(200, None, vec![0; WASM.len()])],
            GrammarState::VerificationFailed,
        ),
    ];
    for (hops, expected) in cases {
        let (directory, registry, clock, transport) = setup(hops);
        assert_eq!(
            registry.request().unwrap_err().state,
            GrammarState::Downloading
        );
        settled(&registry);
        assert_eq!(registry.stored().unwrap_err().state, expected);
        assert_eq!(registry.compile_count(), 0);
        assert!(!stored_path(directory.path(), &PRODUCTION_MANIFEST).exists());
        if expected == GrammarState::VerificationFailed {
            clock.advance(1_000_000);
            assert_eq!(registry.request().unwrap_err().state, expected);
            assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        }
    }
    let (_directory, registry, _, transport) = setup([
        Hop::Response(302, Some("https://second.example/grammar".into()), vec![]),
        success(),
    ]);
    let ready = registry.subscribe();
    registry.request().unwrap_err();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap(),
        registry.identity()
    );
    registry.stored().unwrap();
    assert_eq!(transport.urls.lock()[1], "https://second.example/grammar");
}

#[test]
fn grammar_storage_faults_discard_bytes_then_retry() {
    for fault in [
        StorageFault::Create,
        StorageFault::Admit,
        StorageFault::Write,
        StorageFault::Publish,
    ] {
        let (directory, registry, clock, transport) = setup([success(), success()]);
        registry.set_storage_fault(fault);
        assert_eq!(
            registry.request().unwrap_err().state,
            GrammarState::Downloading
        );
        settled(&registry);
        assert_eq!(
            registry.stored().unwrap_err().state,
            GrammarState::StorageRetry
        );
        assert_eq!(registry.compile_count(), 0);
        if directory.path().join("grammars").exists() {
            assert!(std::fs::read_dir(directory.path().join("grammars"))
                .unwrap()
                .next()
                .is_none());
        }
        registry.set_storage_fault(StorageFault::None);
        assert_eq!(
            registry.request().unwrap_err().state,
            GrammarState::StorageRetry
        );
        clock.advance(30_000);
        registry.request().unwrap_err();
        settled(&registry);
        registry.stored().unwrap();
        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
        assert_eq!(registry.compile_count(), 1);
    }
}

#[test]
fn grammar_altered_and_unreadable_stored_files_count_as_missing() {
    for unreadable in [false, true] {
        let (directory, registry, _, transport) = setup([success()]);
        stored(directory.path());
        if unreadable {
            registry.set_storage_fault(StorageFault::Read);
        } else {
            std::fs::write(
                stored_path(directory.path(), &PRODUCTION_MANIFEST),
                vec![0; WASM.len()],
            )
            .unwrap();
        }
        assert_eq!(
            registry.stored().unwrap_err().state,
            GrammarState::NotDownloaded
        );
        if !unreadable {
            assert!(!stored_path(directory.path(), &PRODUCTION_MANIFEST).exists());
        }
        registry.set_storage_fault(StorageFault::None);
        // An unreadable file is absent to the loader; removing it keeps that
        // condition after the injected read failure is lifted.
        if unreadable {
            std::fs::remove_file(stored_path(directory.path(), &PRODUCTION_MANIFEST)).unwrap();
        }
        registry.request().unwrap_err();
        settled(&registry);
        registry.stored().unwrap();
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn grammar_load_and_query_failures_are_terminal_without_readiness() {
    use sha2::{Digest, Sha256};
    for bad_query in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let bytes = if bad_query {
            WASM
        } else {
            b"not a wasm module"
        };
        let mut manifest = PRODUCTION_MANIFEST.clone();
        manifest.size = bytes.len() as u64;
        manifest.sha256 = format!("{:x}", Sha256::digest(bytes));
        let transport = Script::new([Hop::Response(200, None, bytes.to_vec())]);
        let registry = GrammarRegistry::with_test_options(TestOptions {
            manifest,
            storage_root: directory.path().into(),
            clock: Arc::new(FakeClock::default()),
            transport: transport.clone(),
            queries: vec![if bad_query {
                "(no_such_node) @x"
            } else {
                "(chunk) @root"
            }
            .into()],
            fault: StorageFault::None,
        });
        let ready = registry.subscribe();
        registry.request().unwrap_err();
        settled(&registry);
        assert_eq!(
            registry.request().unwrap_err().state,
            GrammarState::LoadFailed
        );
        assert!(ready.try_recv().is_err());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn grammar_engine_is_fixed_to_pulley_on_every_platform() {
    assert_eq!(engine_target().unwrap(), "pulley");
    assert!(ENGINE
        .as_ref()
        .expect("Pulley engine must be available")
        .is_pulley());
}

#[test]
fn grammar_concurrent_requests_share_one_background_attempt() {
    struct Gated {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        calls: AtomicUsize,
    }
    impl Transport for Gated {
        fn get(&self, _: &reqwest::Url, _: Duration) -> Result<Response, DownloadFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            Ok(Response {
                status: 200,
                location: None,
                body: Box::new(Cursor::new(WASM)),
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let (entered, rx) = std::sync::mpsc::channel();
    let (release, receiver) = std::sync::mpsc::channel();
    let transport = Arc::new(Gated {
        entered,
        release: Mutex::new(receiver),
        calls: AtomicUsize::new(0),
    });
    let registry = GrammarRegistry::with_test_options(TestOptions {
        manifest: PRODUCTION_MANIFEST.clone(),
        storage_root: directory.path().into(),
        clock: Arc::new(FakeClock::default()),
        transport: transport.clone(),
        queries: vec!["(chunk) @root".into()],
        fault: StorageFault::None,
    });
    let ready = registry.subscribe();
    assert_eq!(
        registry.request().unwrap_err().state,
        GrammarState::Downloading
    );
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..24 {
            let registry = &registry;
            scope.spawn(move || {
                assert_eq!(
                    registry.request().unwrap_err().state,
                    GrammarState::Downloading
                )
            });
        }
    });
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap(),
        registry.identity()
    );
    registry.request().unwrap();
    assert_eq!(registry.compile_count(), 1);
}

#[test]
fn grammar_deadline_and_partial_transport_errors_are_network_failures() {
    struct DeadlineTransport(Arc<AtomicU64>);
    struct DeadlineRead(Arc<AtomicU64>);
    impl std::io::Read for DeadlineRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.0.store(40_000, Ordering::SeqCst);
            Ok(0)
        }
    }
    impl Transport for DeadlineTransport {
        fn get(&self, _: &reqwest::Url, _: Duration) -> Result<Response, DownloadFailure> {
            Ok(Response {
                status: 200,
                location: None,
                body: Box::new(DeadlineRead(self.0.clone())),
            })
        }
    }
    let time = Arc::new(AtomicU64::new(0));
    let transport = DeadlineTransport(time.clone());
    let result = download::fetch_timed(&PRODUCTION_MANIFEST, &transport, &|| {
        Duration::from_millis(time.load(Ordering::SeqCst))
    });
    assert_eq!(result.unwrap_err(), DownloadFailure::Network);
    struct PartialTransport;
    impl Transport for PartialTransport {
        fn get(&self, _: &reqwest::Url, _: Duration) -> Result<Response, DownloadFailure> {
            struct BrokenRead;
            impl std::io::Read for BrokenRead {
                fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "injected interrupted body",
                    ))
                }
            }
            Ok(Response {
                status: 200,
                location: None,
                body: Box::new(BrokenRead),
            })
        }
    }
    assert_eq!(
        download::fetch(&PRODUCTION_MANIFEST, &PartialTransport).unwrap_err(),
        DownloadFailure::Network
    );
}

#[test]
fn grammar_configured_cache_uses_flat_artifact_domain_without_durable_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let durable = directory.path().join("durable");
    let cache = directory.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let mut config = crate::config::Config {
        storage_dir: Some(durable.clone()),
        cache_dir: Some(cache.clone()),
        ..Default::default()
    };
    assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
    let names = || {
        let mut names = std::fs::read_dir(&durable)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let before = names();
    let registry = GrammarRegistry::with_test_options(TestOptions {
        manifest: PRODUCTION_MANIFEST.clone(),
        storage_root: crate::root_cache::cache_root(&config),
        clock: Arc::new(FakeClock::default()),
        transport: Script::new([success()]),
        queries: vec!["(chunk) @root".into()],
        fault: StorageFault::None,
    });
    registry.request().unwrap_err();
    settled(&registry);
    registry.stored().unwrap();
    assert!(stored_path(&cache.join("aft"), &PRODUCTION_MANIFEST).is_file());
    assert!(!durable.join("grammars").exists());
    assert_eq!(names(), before);
    assert!(crate::root_cache::CACHE_DOMAINS.contains(&"grammars"));
}

#[test]
fn grammar_storage_exhaustion_reports_last_storage_cause() {
    let (_directory, registry, clock, transport) = setup((0..5).map(|_| success()));
    registry.set_storage_fault(StorageFault::Publish);
    for attempt in 0..5 {
        assert_eq!(
            registry.request().unwrap_err().state,
            GrammarState::Downloading
        );
        settled(&registry);
        clock.advance(30_000 << attempt);
    }
    let error = registry.request().unwrap_err();
    assert_eq!(error.state, GrammarState::AttemptsExhausted);
    assert_eq!(error.cause, Some(FailureCause::Storage));
    assert_eq!(error.retry_after_ms, None);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 5);
    assert_eq!(registry.compile_count(), 0);
}
