//! Rollback safety: current code meeting artifacts in a newer on-disk format.
//!
//! Each test plants one artifact whose version header is above what this build
//! reads (the "future" build's output), configures current code over it, and
//! checks three things: the refusal names the artifact, the artifact's bytes
//! are unchanged afterwards (no rebuild, rewrite or removal over it), and the
//! owning component reports itself unavailable with that refusal as the reason.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::AppContext;
use aft::feature_status::{observed_index_status, IndexEffective, IndexPlane};
use aft::parser::TreeSitterProvider;
use aft::persisted_format::{PersistedStore, CODE};
use aft::protocol::RawRequest;
use serde_json::{json, Value};

const FUTURE: u32 = 99;
const SETTLE: Duration = Duration::from_secs(15);

struct Fixture {
    _project_dir: tempfile::TempDir,
    _storage_dir: tempfile::TempDir,
    project: PathBuf,
    storage: PathBuf,
    key: String,
}

impl Fixture {
    fn new() -> Self {
        let project_dir = tempfile::tempdir().expect("project dir");
        let storage_dir = tempfile::tempdir().expect("storage dir");
        fs::write(
            project_dir.path().join("lib.rs"),
            "pub fn greet() -> &'static str { \"hi\" }\npub fn main() { greet(); }\n",
        )
        .expect("write source");
        let project = fs::canonicalize(project_dir.path()).expect("canonical project");
        let storage = storage_dir.path().to_path_buf();
        let key = aft::search_index::artifact_cache_key(&project);
        Self {
            _project_dir: project_dir,
            _storage_dir: storage_dir,
            project,
            storage,
            key,
        }
    }

    fn artifact(&self, dir: &str, file: &str) -> PathBuf {
        let path = self.storage.join(dir).join(&self.key).join(file);
        fs::create_dir_all(path.parent().unwrap()).expect("artifact dir");
        path
    }

    fn configure(&self, indexes: Value) -> Arc<AppContext> {
        self.configure_with_embeddings(indexes, "http://127.0.0.1:9")
    }

    fn configure_with_embeddings(&self, indexes: Value, embedding_url: &str) -> Arc<AppContext> {
        let ctx = Arc::new(
            AppContext::new(
                Box::new(TreeSitterProvider::new()),
                crate::context_storage::isolate(Config::default()),
            )
            .with_test_checkpoint_store(),
        );
        let mut doc = json!({ "indexes": indexes, "views": { "enabled": false } });
        if indexes["semantic"] == true {
            doc["semantic"] = json!({
                "backend": "openai_compatible",
                "model": "rollback-probe",
                "base_url": embedding_url,
                "timeout_ms": 2_000,
            });
        }
        let request: RawRequest = serde_json::from_value(json!({
            "id": "configure-rollback-safety",
            "command": "configure",
            "harness": "opencode",
            "session_id": "rollback-safety",
            "project_root": self.project,
            "storage_dir": self.storage,
            "config": crate::helpers::user_config(doc),
        }))
        .expect("configure request");
        let response = aft::commands::configure::handle_configure(&request, &ctx);
        assert!(response.success, "configure failed: {response:?}");
        aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
        if indexes
            .as_object()
            .is_some_and(|planes| planes.values().any(|on| on == true))
        {
            assert_eq!(
                ctx.cached_artifact_cache_key(&self.project).as_deref(),
                Some(self.key.as_str()),
                "the planted artifact must sit under the key configure actually uses"
            );
        }
        ctx
    }
}

/// The search lane published an index: a built one or a denied (empty) one.
fn search_settled(ctx: &AppContext) -> bool {
    ctx.search_index().read().ok().is_some_and(|index| {
        index
            .as_ref()
            .is_some_and(|index| index.ready || index.build_denied)
    })
}

/// A minimal OpenAI-compatible embedding endpoint, so a semantic cold build
/// could actually run (and persist) if the refusal did not stop it first.
fn start_embedding_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let url = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 8192];
                let body = loop {
                    let Ok(read) = stream.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let length = String::from_utf8_lossy(&bytes[..end])
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break bytes[end + 4..end + 4 + length].to_vec();
                    }
                };
                let request: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                let inputs = match &request["input"] {
                    Value::Array(values) => values.len(),
                    Value::String(_) => 1,
                    _ => 0,
                };
                let data = (0..inputs)
                    .map(|index| json!({ "embedding": [0.1, 0.2, 0.3], "index": index }))
                    .collect::<Vec<_>>();
                let body = json!({ "data": data }).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            });
        }
    });
    url
}

fn drain(ctx: &AppContext) {
    aft::runtime_drain::drain_search_index_events(ctx);
    aft::runtime_drain::drain_semantic_index_events(ctx);
    aft::runtime_drain::drain_callgraph_store_events(ctx);
}

/// Wait until `done` holds (the component settled after refusing), draining
/// background events meanwhile.
fn settle(ctx: &AppContext, what: &str, mut done: impl FnMut(&AppContext) -> bool) {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        drain(ctx);
        if done(ctx) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{what} did not settle within {SETTLE:?}; semantic status {:?}",
        ctx.semantic_index_status()
            .read()
            .map(|status| format!("{status:?}"))
    );
}

fn assert_named(reason: &str, store: PersistedStore, path: &Path) {
    assert!(
        reason.contains(CODE),
        "reason must carry the code: {reason}"
    );
    assert!(
        reason.contains(store.name()),
        "reason must name {store}: {reason}"
    );
    assert!(
        reason.contains(&path.display().to_string()),
        "reason must name {}: {reason}",
        path.display()
    );
    assert!(
        reason.contains(&FUTURE.to_string()),
        "reason must carry the version: {reason}"
    );
}

fn assert_plane_refused(ctx: &AppContext, plane: IndexPlane, store: PersistedStore, path: &Path) {
    let observation = observed_index_status(ctx, plane);
    assert_eq!(
        observation.effective,
        IndexEffective::Unavailable,
        "{observation:?}"
    );
    assert_named(
        observation
            .unavailable_reason
            .as_deref()
            .unwrap_or_default(),
        store,
        path,
    );
}

fn assert_status_lists(ctx: &AppContext, store: PersistedStore) -> Value {
    let status = ctx.build_status_snapshot();
    let reason = format!("{CODE}:{}", store.name());
    assert!(
        status["degraded_reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.iter().any(|value| value == &json!(reason))),
        "degraded_reasons must name {reason}: {}",
        status["degraded_reasons"]
    );
    assert!(
        status["storage_refusals"]
            .as_array()
            .is_some_and(|refusals| refusals
                .iter()
                .any(|refusal| refusal["store"] == store.name())),
        "storage_refusals must list {store}: {}",
        status["storage_refusals"]
    );
    status
}

/// Status names a refused data-layer index store as today's stores are
/// named: `storage_requires_newer_reader:index_store` among the degraded
/// reasons, and a `storage_refusals` entry for the refused file.
fn assert_status_lists_index_store(ctx: &AppContext, path: &Path) -> Value {
    let status = ctx.build_status_snapshot();
    let reason = format!("{CODE}:{}", aft::data_layer::checkout::INDEX_STORE);
    assert_eq!(reason, "storage_requires_newer_reader:index_store");
    assert!(
        status["degraded_reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.iter().any(|value| value == &json!(reason))),
        "degraded_reasons must name {reason}: {}",
        status["degraded_reasons"]
    );
    let entry = json!({
        "code": CODE,
        "store": "index_store",
        "path": path.display().to_string(),
        "found": FUTURE,
        "supported": 1,
        "message": format!(
            "{CODE}: index_store at {} has format version {FUTURE}; this build reads up to 1",
            path.display()
        ),
    });
    assert!(
        status["storage_refusals"]
            .as_array()
            .is_some_and(|refusals| refusals.contains(&entry)),
        "storage_refusals must list {entry}: {}",
        status["storage_refusals"]
    );
    for plane in ["search_index", "callgraph_store"] {
        assert_eq!(
            status[plane]["status"], "unavailable",
            "{plane}: {status:#}"
        );
        assert_eq!(status[plane]["reason"], CODE, "{plane}: {status:#}");
    }
    assert_eq!(status["data_store"]["error"], entry["message"]);
    status
}

/// The names in `dir` and every folder under it, sorted.
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut names = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(folder) = pending.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            names.push(path.strip_prefix(dir).unwrap().to_path_buf());
        }
    }
    names.sort();
    names
}

/// The names in the store folders an older build writes to or reclaims
/// from: everything but the leases a reader takes while it reads.
fn written(family_dir: &Path) -> Vec<PathBuf> {
    tree(family_dir)
        .into_iter()
        .filter(|path| !path.starts_with("leases") && !path.starts_with("incoming"))
        .collect()
}

/// The data layer's index store (`<storage>/data/<family>/`): a manifest a
/// newer build published is refused, left as it is, and named on status.
#[test]
fn future_index_store_manifest_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    // Publish the store as this build writes it, then plant the next
    // generation's manifest as a newer build would write it.
    aft::data_layer::surfaces::index_root(
        &fixture.project,
        &fixture.storage,
        aft::data_layer::checkout::Planes::ALL,
    )
    .expect("index the fixture");
    let families = fs::read_dir(fixture.storage.join("data"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(families.len(), 1, "one family store: {families:?}");
    let family_dir = &families[0];
    let listing = |dir: &Path| {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let newest = listing(family_dir)
        .iter()
        .filter_map(|name| name.strip_prefix("MANIFEST-")?.parse::<u64>().ok())
        .max()
        .expect("index_root published a manifest");
    // A record is a 48-byte header (magic, kind, the kind's format version
    // at bytes 10..12, flags, family, the number in its name at bytes
    // 32..40, payload length), the payload, and a BLAKE3-128 checksum of
    // everything before it; see aft-store's family/record.rs.
    let mut bytes = fs::read(family_dir.join(format!("MANIFEST-{newest}"))).unwrap();
    let future_version = u16::try_from(FUTURE).unwrap();
    bytes[10..12].copy_from_slice(&future_version.to_le_bytes());
    bytes[32..40].copy_from_slice(&(newest + 1).to_le_bytes());
    let body = bytes.len() - aft_store::ids::Digest128::LEN;
    let checksum = aft_store::ids::Digest128::of(&bytes[..body]);
    bytes[body..].copy_from_slice(checksum.as_bytes());
    let planted = family_dir.join(format!("MANIFEST-{}", newest + 1));
    fs::write(&planted, &bytes).unwrap();
    let before = written(family_dir);

    let ctx = fixture.configure(json!({ "trigram": true, "semantic": false, "callgraph": true }));
    // The first query of the root binds its checkout in the data layer: it
    // opens the index store and reads the newest manifest there.
    let grep: RawRequest = serde_json::from_value(json!({
        "id": "grep-rollback-safety",
        "command": "grep",
        "pattern": "greet",
    }))
    .expect("grep request");
    let response = aft::commands::grep::handle_grep(&grep, &ctx);
    assert!(
        response.success,
        "grep still answers, by walking: {:?}",
        response.data
    );
    settle(&ctx, "index store", |ctx| {
        ctx.build_status_snapshot()["search_index"]["status"] == "unavailable"
    });

    assert_eq!(
        fs::read(&planted).unwrap(),
        bytes,
        "newer manifest was changed"
    );
    assert_eq!(
        written(family_dir),
        before,
        "a generation, segment or state was written, or a file removed"
    );
    // The refusal is named as today's stores name theirs: in the degraded
    // reasons and storage refusals, and as the reason both planes are
    // unavailable, rather than as a failed bind.
    assert_status_lists_index_store(&ctx, &planted);
}

/// The data layer's index store: a segment in a newer format, named by a
/// manifest this build reads, is refused by name, never rebuilt over or
/// reclaimed, and named on status; a foreground build of the root fails
/// with the same refusal.
#[test]
fn future_index_store_segment_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    aft::data_layer::surfaces::index_root(
        &fixture.project,
        &fixture.storage,
        aft::data_layer::checkout::Planes::ALL,
    )
    .expect("index the fixture");
    let families = fs::read_dir(fixture.storage.join("data"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(families.len(), 1, "one family store: {families:?}");
    let family_dir = &families[0];
    // A segment's header names its format version at bytes 8..10, which a
    // reader checks before anything else after the magic; see aft-store's
    // segment/format.rs. Every segment on disk gets the newer version, so
    // the one the newest manifest names is among them; their lengths, which
    // the manifest published, are unchanged.
    let mut planted = fs::read_dir(family_dir.join("segments"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let mut bytes = fs::read(&path).unwrap();
            bytes[8..10].copy_from_slice(&u16::try_from(FUTURE).unwrap().to_le_bytes());
            fs::remove_file(&path).unwrap();
            fs::write(&path, &bytes).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    planted.sort();
    assert!(!planted.is_empty(), "index_root published a segment");
    let before = written(family_dir);
    let unchanged = |when: &str| {
        for (path, bytes) in &planted {
            assert_eq!(
                &fs::read(path).unwrap(),
                bytes,
                "{} was changed {when}",
                path.display()
            );
        }
        assert_eq!(
            written(family_dir),
            before,
            "a file was written or removed {when}"
        );
    };

    let ctx = fixture.configure(json!({ "trigram": true, "semantic": false, "callgraph": true }));
    let grep: RawRequest = serde_json::from_value(json!({
        "id": "grep-rollback-safety",
        "command": "grep",
        "pattern": "greet",
    }))
    .expect("grep request");
    let response = aft::commands::grep::handle_grep(&grep, &ctx);
    assert!(
        response.success,
        "grep still answers, by walking: {:?}",
        response.data
    );
    settle(&ctx, "index store", |ctx| {
        ctx.build_status_snapshot()["search_index"]["status"] == "unavailable"
    });
    unchanged("by the bind");
    let status = ctx.build_status_snapshot();
    let refused = status["storage_refusals"]
        .as_array()
        .and_then(|refusals| {
            refusals
                .iter()
                .find(|refusal| refusal["store"] == "index_store")
        })
        .and_then(|refusal| refusal["path"].as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("no index_store refusal: {status:#}"));
    assert!(
        planted.iter().any(|(path, _)| *path == refused),
        "the refusal names {}, not a planted segment",
        refused.display()
    );
    assert_status_lists_index_store(&ctx, &refused);

    // A foreground build, as `aft index` runs it, fails with the refusal and
    // neither rebuilds over the segment nor reclaims it.
    let error = aft::data_layer::surfaces::index_root(
        &fixture.project,
        &fixture.storage,
        aft::data_layer::checkout::Planes::ALL,
    )
    .expect_err("the build is refused");
    assert_named_index_store(&error, &refused);
    unchanged("by the foreground build");
}

/// As [`assert_named`], for the data layer's index store.
fn assert_named_index_store(reason: &str, path: &Path) {
    assert_eq!(
        reason,
        format!(
            "{CODE}: index_store at {} has format version {FUTURE}; this build reads up to 1",
            path.display()
        )
    );
}

#[test]
fn future_semantic_index_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    let snapshot = fixture.artifact("semantic", "semantic.bin");
    let mut bytes = vec![FUTURE as u8];
    bytes.extend_from_slice(&[0x5A; 256]);
    fs::write(&snapshot, &bytes).unwrap();

    // A working embedding backend: without the refusal, the lane would cold
    // build and persist a fresh snapshot over the newer one.
    let embeddings = start_embedding_server();
    let ctx = fixture.configure_with_embeddings(
        json!({ "trigram": false, "semantic": true, "callgraph": false }),
        &embeddings,
    );
    settle(&ctx, "semantic index", |ctx| {
        matches!(
            *ctx.semantic_index_status().read().unwrap(),
            aft::context::SemanticIndexStatus::Failed(_)
                | aft::context::SemanticIndexStatus::Ready { .. }
        )
    });
    // Let a persist that follows a (wrongly) finished build reach the disk.
    thread::sleep(Duration::from_millis(500));

    assert_eq!(
        fs::read(&snapshot).unwrap(),
        bytes,
        "newer semantic snapshot was changed"
    );
    assert_plane_refused(
        &ctx,
        IndexPlane::Semantic,
        PersistedStore::SemanticIndex,
        &snapshot,
    );
    let status = assert_status_lists(&ctx, PersistedStore::SemanticIndex);
    assert_eq!(status["semantic_index"]["status"], "unavailable");
    assert_named(
        status["semantic_index"]["reason"]
            .as_str()
            .unwrap_or_default(),
        PersistedStore::SemanticIndex,
        &snapshot,
    );
}

#[test]
fn future_callgraph_generation_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    let generation = format!("{}.g1.1.sqlite", fixture.key);
    let generation_path = fixture.artifact("callgraph", &generation);
    {
        let conn = rusqlite::Connection::open(&generation_path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             INSERT INTO meta(k, v) VALUES ('schema_version', '{FUTURE}');
             INSERT INTO meta(k, v) VALUES ('fingerprint', 'written-by-a-newer-build');
             INSERT INTO meta(k, v) VALUES ('ready', '1');"
        ))
        .unwrap();
    }
    let pointer = fixture.artifact("callgraph", &format!("{}.current", fixture.key));
    fs::write(&pointer, &generation).unwrap();
    let generation_bytes = fs::read(&generation_path).unwrap();
    let callgraph_dir = generation_path.parent().unwrap().to_path_buf();
    let listing = |dir: &Path| {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sqlite") || name.ends_with(".current"))
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let files_before = listing(&callgraph_dir);

    let ctx = fixture.configure(json!({ "trigram": false, "semantic": false, "callgraph": true }));
    // Asking for the store is what would cold-build and republish the
    // pointer; it must answer with the refusal instead.
    let access = ctx.callgraph_store_for_ops();
    let error = match access {
        aft::context::CallgraphStoreAccess::Error(error) => error.to_string(),
        aft::context::CallgraphStoreAccess::Ready(_) => panic!("the newer store was opened"),
        aft::context::CallgraphStoreAccess::Building => panic!("a build was started over it"),
        _ => panic!("expected the named refusal"),
    };
    assert_named(&error, PersistedStore::CallgraphStore, &generation_path);
    settle(&ctx, "callgraph", |ctx| {
        ctx.callgraph_store_rx()
            .try_lock()
            .is_some_and(|rx| rx.is_none())
    });

    assert_eq!(fs::read(&generation_path).unwrap(), generation_bytes);
    assert_eq!(
        fs::read_to_string(&pointer).unwrap(),
        generation,
        "pointer was republished"
    );
    assert_eq!(
        listing(&callgraph_dir),
        files_before,
        "a new generation was published"
    );
    assert_plane_refused(
        &ctx,
        IndexPlane::Callgraph,
        PersistedStore::CallgraphStore,
        &generation_path,
    );
    assert_status_lists(&ctx, PersistedStore::CallgraphStore);
}

#[test]
fn future_symbol_cache_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    let cache = fixture.artifact("symbols", "symbols.bin");
    let mut bytes = b"AFTSYM1\0".to_vec();
    bytes.extend_from_slice(&FUTURE.to_le_bytes());
    bytes.extend_from_slice(&4_u32.to_le_bytes());
    bytes.extend_from_slice(&[0x3C; 64]);
    fs::write(&cache, &bytes).unwrap();

    // The search index load prewarms (and persists) the symbol cache.
    let ctx = fixture.configure(json!({ "trigram": true, "semantic": false, "callgraph": false }));
    settle(&ctx, "search index", search_settled);
    // Give the prewarm, which runs after the index is published, time to
    // reach its persist step.
    thread::sleep(Duration::from_millis(500));

    assert_eq!(
        fs::read(&cache).unwrap(),
        bytes,
        "newer symbol cache was changed"
    );
    let status = assert_status_lists(&ctx, PersistedStore::SymbolCache);
    let refusal = status["storage_refusals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|refusal| refusal["store"] == "symbols")
        .unwrap();
    assert_named(
        refusal["message"].as_str().unwrap(),
        PersistedStore::SymbolCache,
        &cache,
    );
}

#[test]
fn future_owner_manifest_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    let manifest = fixture.artifact("artifact-owners", "owner.json");
    let bytes = serde_json::to_vec_pretty(&json!({
        "schema_version": FUTURE,
        "project_scope_key": "written-by-a-newer-build",
        "checkout_path": "/nonexistent/newer",
        "owner": { "shape": "changed" },
    }))
    .unwrap();
    fs::write(&manifest, &bytes).unwrap();

    let ctx = fixture.configure(json!({ "trigram": true, "semantic": false, "callgraph": false }));
    drain(&ctx);

    assert_eq!(
        fs::read(&manifest).unwrap(),
        bytes,
        "newer owner manifest was changed"
    );
    let status = assert_status_lists(&ctx, PersistedStore::ArtifactOwner);
    assert_eq!(status["artifact_owner"]["mode"], "read_only");
    assert_named(
        status["artifact_owner"]["note"]
            .as_str()
            .unwrap_or_default(),
        PersistedStore::ArtifactOwner,
        &manifest,
    );
}

#[test]
fn future_aft_db_is_refused_by_name_and_left_byte_identical() {
    let fixture = Fixture::new();
    let db_path = fixture.storage.join("aft.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE schema_version (version INTEGER NOT NULL PRIMARY KEY);
             INSERT INTO schema_version(version) VALUES ({FUTURE});
             CREATE TABLE written_by_a_newer_build (x INTEGER);"
        ))
        .unwrap();
    }
    let bytes = fs::read(&db_path).unwrap();

    let ctx = fixture.configure(json!({ "trigram": false, "semantic": false, "callgraph": false }));

    assert_eq!(
        fs::read(&db_path).unwrap(),
        bytes,
        "newer aft.db was changed"
    );
    assert!(
        !db_path.with_extension("db-wal").exists(),
        "no read-write connection may switch the newer database to WAL"
    );
    let refused = ctx
        .database_runtime_refusal("probe", "write")
        .expect("persistence-dependent tools must be refused");
    let refused = serde_json::to_value(refused).unwrap();
    assert_eq!(refused["code"], "database_unavailable", "{refused}");
    assert_named(
        refused["message"].as_str().unwrap_or_default(),
        PersistedStore::AftDb,
        &db_path,
    );
    assert_status_lists(&ctx, PersistedStore::AftDb);
}

#[test]
fn a_reader_floor_above_this_build_refuses_the_store_before_anything_is_written() {
    let fixture = Fixture::new();
    let above = u64::from(PersistedStore::SearchIndex.supported()) + 1;
    let floor = fixture.storage.join(aft::reader_floor::FLOOR_FILE);
    fs::write(
        &floor,
        serde_json::to_vec(&json!({ "floor_schema": 1, "stores": { "search": above } })).unwrap(),
    )
    .unwrap();

    let ctx = fixture.configure(json!({ "trigram": true, "semantic": false, "callgraph": false }));
    settle(&ctx, "search index", search_settled);

    let cache = fixture
        .storage
        .join("index")
        .join(&fixture.key)
        .join("cache.bin");
    assert!(
        !cache.exists(),
        "a build below the floor must not write the store"
    );
    let observation = observed_index_status(&ctx, IndexPlane::Trigram);
    let reason = observation.unavailable_reason.unwrap_or_default();
    assert!(reason.starts_with(CODE), "{reason}");
    assert!(reason.contains(&floor.display().to_string()), "{reason}");
    let written = aft::reader_floor::read(&fixture.storage).unwrap().unwrap();
    assert_eq!(
        written.get(PersistedStore::SearchIndex),
        Some(above),
        "floor lowered"
    );
}
