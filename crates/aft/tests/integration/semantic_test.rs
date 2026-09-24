use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

mod aft {
    pub mod search_index {
        use std::fs;
        use std::path::Path;

        use sha2::{Digest, Sha256};

        pub fn artifact_cache_key(project_root: &Path) -> String {
            let canonical_root =
                fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
            let mut hasher = Sha256::new();
            hasher.update(canonical_root.to_string_lossy().as_bytes());
            let digest = format!("{:x}", hasher.finalize());
            digest[..16].to_string()
        }
    }
}

use aft::search_index::artifact_cache_key;
use serde_json::{json, Value};

use crate::helpers::{user_config, AftProcess};

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn configure_semantic(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    enabled: bool,
) -> Value {
    send(
        aft,
        json!({
            "id": "cfg-semantic",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "storage_dir": storage_dir.display().to_string(),
            // With semantic off, keep the trigram index off as well: these
            // tests assert the bounded-walk ("literal") fallback, and a trigram
            // index that finishes building before the query would instead
            // answer through the ranked lexical engine.
            "config": user_config(if enabled {
                serde_json::json!({ "indexes": { "semantic": true } })
            } else {
                serde_json::json!({ "indexes": { "semantic": false, "trigram": false } })
            }),
        }),
    )
}

fn wait_for_ready_search(aft: &mut AftProcess, query: &str) -> Value {
    for _ in 0..180 {
        let response = send(
            aft,
            json!({
                "id": "semantic-search",
                "command": "semantic_search",
                "query": query,
                "top_k": 5,
            }),
        );

        assert_eq!(
            response["success"], true,
            "semantic_search should succeed while polling: {response:?}"
        );

        if response["status"] == "ready" {
            return response;
        }

        thread::sleep(Duration::from_millis(250));
    }

    panic!("semantic index did not become ready in time");
}

#[test]
fn semantic_search_refuses_before_any_index_lane_is_ready() {
    // With no ready index lane aft_search refuses with search_lanes_unavailable
    // and each lane's status instead of a degraded lexical grep. Use an empty
    // project directory so the path is deterministic regardless of cwd.
    let project = setup_project(&[]);
    let previous_cwd = std::env::current_dir().expect("read cwd");
    std::env::set_current_dir(project.path()).expect("set cwd to empty project");

    let mut aft = AftProcess::spawn();

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-disabled-fallback",
            "command": "semantic_search",
            "query": "how does request handling work",
        }),
    );

    std::env::set_current_dir(&previous_cwd).expect("restore cwd");

    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "search_lanes_unavailable");
    assert_eq!(response["results"], json!([]));
    assert!(response["lanes"]["trigram"]["status"].is_string());
    assert!(response["lanes"]["semantic"]["status"].is_string());

    let status = aft.shutdown();
    assert!(status.success());
}
#[test]
fn semantic_search_with_both_index_lanes_off_refuses_with_no_search_lanes_enabled() {
    let project = setup_project(&[("src/lib.rs", "pub fn handle_request() -> bool { true }\n")]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let mut aft = AftProcess::spawn();

    let configure = configure_semantic(&mut aft, project.path(), storage.path(), false);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-disabled",
            "command": "semantic_search",
            "query": "how does request handling work",
        }),
    );

    // Registration is unaffected: the tool answers, naming both lanes off.
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "no_search_lanes_enabled");
    assert_eq!(response["lanes"]["trigram"]["status"], "off");
    assert_eq!(response["lanes"]["semantic"]["status"], "off");

    let status = aft.shutdown();
    assert!(status.success());
}
#[test]
#[ignore = "requires fastembed model download (~22MB) and a full semantic index build"]
fn semantic_index_persists_across_configure_build_search_roundtrip() {
    let project = setup_project(&[
        (
            "src/lib.rs",
            "pub fn handle_request(token: &str) -> bool {\n    !token.is_empty()\n}\n\npub struct AuthService;\n",
        ),
        (
            "src/utils.rs",
            "pub fn normalize_user_id(input: &str) -> String {\n    input.trim().to_lowercase()\n}\n",
        ),
    ]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let project_key = artifact_cache_key(project.path());
    let semantic_file = storage
        .path()
        .join("semantic")
        .join(&project_key)
        .join("semantic.bin");

    // Slow by design: this may download the embedding model on first use.
    let mut first = AftProcess::spawn();
    let configure = configure_semantic(&mut first, project.path(), storage.path(), true);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let first_response = wait_for_ready_search(&mut first, "request authentication handler");
    assert_eq!(first_response["status"], "ready");
    assert!(
        semantic_file.is_file(),
        "semantic index should persist to disk"
    );

    let first_results = first_response["results"]
        .as_array()
        .expect("semantic results array");
    assert!(
        !first_results.is_empty(),
        "expected at least one semantic result"
    );
    assert_eq!(first_results[0]["name"], "handle_request");
    assert_eq!(first_results[0]["source"], "semantic");

    let status = first.shutdown();
    assert!(status.success());

    let mut second = AftProcess::spawn();
    let configure = configure_semantic(&mut second, project.path(), storage.path(), true);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let second_response = wait_for_ready_search(&mut second, "request authentication handler");
    assert_eq!(second_response["status"], "ready");
    assert_eq!(second_response["text"], first_response["text"]);
    assert_eq!(second_response["results"], first_response["results"]);

    let status = second.shutdown();
    assert!(status.success());
}

/// Regression for the v0.19.5 fix: Ollama's default `base_url`
/// (`http://127.0.0.1:11434`) and `http://localhost:11434` must be accepted at
/// configure time. Earlier versions rejected all loopback as an SSRF guard,
/// which made the Ollama backend unusable at its default config.
#[test]
fn configure_accepts_loopback_base_url_for_self_hosted_backends() {
    let project = setup_project(&[("src/lib.rs", "pub fn handle_request() {}\n")]);
    let storage = tempfile::tempdir().expect("create storage dir");

    for base_url in &[
        "http://127.0.0.1:11434", // Ollama default
        "http://localhost:11434",
        "http://127.0.0.1:8080",
    ] {
        let mut aft = AftProcess::spawn();
        let response = send(
            &mut aft,
            json!({
                "id": "cfg-ollama",
                "command": "configure",
                "harness": "opencode",
                "project_root": project.path().display().to_string(),
                "storage_dir": storage.path().display().to_string(),
                "config": user_config(serde_json::json!({
                    "semantic_search": true,
                    "semantic": {
                        "backend": "ollama",
                        "model": "nomic-embed-text",
                        "base_url": base_url
                    }
                })),
            }),
        );
        assert_eq!(
            response["success"], true,
            "configure should accept loopback base_url {base_url}, got: {response:?}"
        );
        let _ = aft.shutdown();
    }
}

/// User-tier semantic endpoints are trusted user config, so self-hosted
/// private-network embedding backends are accepted through the tier resolver.
#[test]
fn configure_accepts_user_private_base_url() {
    let project = setup_project(&[("src/lib.rs", "pub fn handle_request() {}\n")]);
    let storage = tempfile::tempdir().expect("create storage dir");

    for base_url in &[
        "http://192.168.1.50:8080",
        "http://10.0.0.5:11434",
        "http://172.16.0.10:8080",
    ] {
        let mut aft = AftProcess::spawn();
        let response = send(
            &mut aft,
            json!({
                "id": "cfg-private",
                "command": "configure",
                "harness": "opencode",
                "project_root": project.path().display().to_string(),
                "storage_dir": storage.path().display().to_string(),
                "config": user_config(serde_json::json!({
                    "semantic_search": true,
                    "semantic": {
                        "backend": "openai_compatible",
                        "model": "text-embedding-3-small",
                        "base_url": base_url,
                        "api_key_env": "FAKE_KEY"
                    }
                })),
            }),
        );
        assert_eq!(
            response["success"], true,
            "configure should accept user private base_url {base_url}, got: {response:?}"
        );
        let _ = aft.shutdown();
    }
}
