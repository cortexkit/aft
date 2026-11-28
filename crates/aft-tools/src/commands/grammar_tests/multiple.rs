use super::*;
use crate::grammar::{self, GrammarRegistry, GrammarState};

#[test]
fn grammar_registries_download_fail_and_retry_independently() {
    if let Some(dir) = std::env::var_os("AFT_TEST_MULTI_GRAMMAR_ROOT") {
        check_registries(Path::new(&dir));
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let fixtures = crate::test_support::manifest_dir().join("../aft/tests/fixtures/grammars");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("manifest-a.json")).unwrap()).unwrap();
    let variant: Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("manifest-b.json")).unwrap()).unwrap();
    let mut fake = variant["grammars"][0].clone();
    fake["language"] = "fake".into();
    fake["extensions"] = json!(["aftfake"]);
    manifest["grammars"].as_array_mut().unwrap().push(fake);
    let manifest_path = write(dir.path(), "manifest.json", manifest.to_string());
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "commands::grammar_tests::multiple::grammar_registries_download_fail_and_retry_independently", "--nocapture"])
        .env("AFT_TEST_MULTI_GRAMMAR_ROOT", dir.path())
        .env("AFT_TEST_GRAMMAR_MANIFEST", manifest_path)
        .env("AFT_TEST_GRAMMAR_PARSE_DEADLINE_MS", "30000")
        .env("AFT_TEST_GRAMMAR_TRANSPORT_DIR", dir.path().join("transport"))
        .env("AFT_TEST_GRAMMAR_CLOCK_FILE", write(dir.path(), "clock", "0"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn check_registries(dir: &Path) {
    let root = dir.join("project");
    let fake_file = write(&root, "separate.aftfake", "fake");
    let luau_file = write(
        &root,
        "function.luau",
        "local function target() return 1 end\n",
    );
    let ctx = context(&root, &dir.join("storage"));
    grammar::configure(&ctx.config());
    let fake = grammar::registry("fake").unwrap();
    let luau = grammar::registry("luau").unwrap();
    assert!(grammar::registry("rust").is_none());
    assert!(!Arc::ptr_eq(fake, luau));
    assert_eq!(
        grammar::manifest_for_path(&fake_file).unwrap().language,
        "fake"
    );
    assert_eq!(crate::parser::detect_language_any(&fake_file, true), None);
    assert_eq!((fake.request_count(), luau.request_count()), (0, 0));
    assert_eq!((fake.compile_count(), luau.compile_count()), (0, 0));
    let luau_ready = luau.subscribe();
    let fake_ready = fake.subscribe();
    let outline = |path: &Path| ask(&ctx, "outline", json!({"file": path}));
    let settled =
        |registry: &GrammarRegistry| assert!(registry.wait_until_settled(Duration::from_secs(30)));

    assert_eq!(outline(&fake_file).data["language"], "fake");
    settled(fake);
    assert_eq!(
        fake.status_snapshot().unwrap().state,
        GrammarState::NetworkRetry
    );
    assert_eq!(
        luau.status_snapshot().unwrap().state,
        GrammarState::NotDownloaded
    );
    assert_eq!((fake.request_count(), luau.request_count()), (1, 0));

    replace_polled(dir, "clock", "10000");
    assert_eq!(outline(&luau_file).data["language"], "luau");
    settled(luau);
    assert_eq!(
        luau.status_snapshot().unwrap().state,
        GrammarState::NetworkRetry
    );
    assert_eq!(fake.status_snapshot().unwrap().retry_after_ms, Some(20_000));

    let fixtures = crate::test_support::manifest_dir().join("../aft/tests/fixtures/grammars");
    let cache = crate::root_cache::cache_root(&ctx.config());
    let supply_artifact = |language| {
        let entry = grammar::manifest(language).unwrap();
        let filename = format!("{}.wasm", entry.sha256);
        let bytes = std::fs::read(fixtures.join(&filename)).unwrap();
        let transport = dir.join("transport");
        std::fs::create_dir_all(&transport).unwrap();
        replace_polled(&transport, &filename, &bytes);
        (grammar::test_seams::stored_path(&cache, entry), bytes)
    };
    let (fake_artifact, fake_bytes) = supply_artifact("fake");
    replace_polled(dir, "clock", "30000");
    outline(&fake_file);
    settled(fake);
    // The fixture exports Luau, not fake: download verification passes but loading cannot.
    assert_eq!(std::fs::read(fake_artifact).unwrap(), fake_bytes);
    assert_eq!(
        fake.status_snapshot().unwrap().state,
        GrammarState::LoadFailed
    );
    assert_eq!(luau.status_snapshot().unwrap().retry_after_ms, Some(10_000));
    assert_eq!((fake.request_count(), luau.request_count()), (2, 1));

    let gap = crate::data_layer::grammar::GapReply::capture();
    let stored = r#"{"code":"grammar_unavailable","state":"not_downloaded"}"#;
    assert_eq!(
        gap.value(stored, &fake_file).unwrap()["state"],
        "load_failed"
    );
    assert_eq!(
        gap.value(stored, &luau_file).unwrap()["state"],
        "network_retry"
    );

    let (luau_artifact, luau_bytes) = supply_artifact("luau");
    replace_polled(dir, "clock", "40000");
    outline(&luau_file);
    settled(luau);
    assert_eq!(
        luau_ready.recv_timeout(Duration::from_secs(2)).unwrap(),
        luau.identity()
    );
    assert!(fake_ready.try_recv().is_err());
    assert_eq!(std::fs::read(luau_artifact).unwrap(), luau_bytes);
    assert_eq!((fake.request_count(), luau.request_count()), (2, 2));
    assert!(luau.status_snapshot().is_none());
    let broken = outline(&fake_file);
    assert_eq!(broken.data["state"], "load_failed");
    let success = outline(&luau_file);
    assert!(success.success, "{:?}", success.data);
    assert!(success.data["text"].as_str().unwrap().contains("target"));
}
