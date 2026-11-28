use super::{hermetic_home, open_with_harness, request, HermeticDaemon};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use subc_client_rs::{ConsumerOptions, SubcConsumer};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const QUESTION: &str = "Where does the spacecraft decide on a safe place to land?";

pub(super) async fn check_route(harness: &str, worker: bool) {
    let (_temp, home, project) = hermetic_home();
    let (base_url, start_embedding, mock) = MockEmbedder::start().await;
    configure(&home, &project, &base_url, worker);
    let daemon = HermeticDaemon::start(&home).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, ConsumerOptions::default())
        .await
        .expect("connect to the hermetic daemon");
    let route = open_with_harness(
        &consumer,
        &project,
        "e2e-project",
        harness,
        "hosted-meaning",
        Some(&[("tool-provider", "v1")]),
    )
    .await
    .expect("bind daemon-owned project scope");
    let readiness = if worker {
        start_embedding.send(()).unwrap();
        None
    } else {
        // Runner routes do not receive status pushes. Observe the same root
        // through a Worker route that cannot open meaning search itself.
        let observer = open_with_harness(
            &consumer,
            &project,
            "e2e-status-observer",
            "mcp:status-observer",
            "meaning-readiness",
            None,
        )
        .await
        .expect("bind status observer");
        let mut pushes = consumer
            .push_events(&observer)
            .expect("subscribe to AFT pushes");
        // Keep the fill from finishing before the observer is listening.
        start_embedding.send(()).unwrap();
        Some(
            tokio::time::timeout(Duration::from_secs(25), async {
                while let Some(push) = pushes.recv().await {
                    let event: Value = serde_json::from_slice(&push.body).unwrap();
                    if event["type"] == "status_changed"
                        && event["snapshot"]["semantic_index"]["status"] == "ready"
                    {
                        return true;
                    }
                }
                false
            })
            .await,
        )
    };
    let search = request(
        &consumer,
        &route,
        json!({
            "op": "tool.call",
            "name": if worker { "search" } else { "aft_search" },
            "preset": if worker { "worker" } else { "head" },
            "arguments": {"query": QUESTION},
        }),
    )
    .await
    .expect("search through the daemon route");
    let search = String::from_utf8(search).expect("search reply is text or JSON");
    if worker {
        assert!(search.contains("runtime_not_observed"), "{search}");
        assert_eq!(mock.calls.embeds.load(Ordering::Relaxed), 0, "{search}");
    } else {
        assert!(
            search.contains("semantic: ready") && search.contains("src/lib.rs"),
            "{harness} Head route did not serve a meaning result: {search}"
        );
        assert!(
            matches!(readiness, Some(Ok(true))),
            "{harness} readiness push"
        );
        assert!(mock.calls.queries.load(Ordering::Relaxed) > 0, "{search}");
    }
}

#[derive(Default)]
struct EmbeddingCalls {
    embeds: AtomicUsize,
    queries: AtomicUsize,
}

struct MockEmbedder {
    task: tokio::task::JoinHandle<()>,
    calls: Arc<EmbeddingCalls>,
}

impl MockEmbedder {
    async fn start() -> (String, oneshot::Sender<()>, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (start, started) = oneshot::channel();
        let calls = Arc::new(EmbeddingCalls::default());
        let metrics = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            if started.await.is_ok() {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    serve_embedding(stream, &metrics).await.unwrap();
                }
            }
        });
        (url, start, Self { task, calls })
    }
}

impl Drop for MockEmbedder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_embedding(stream: TcpStream, calls: &EmbeddingCalls) -> std::io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    let mut length = None;
    while stream.read_line(&mut line).await? != 0 && line != "\r\n" {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse().unwrap());
            }
        }
        line.clear();
    }
    let mut body = vec![0; length.expect("embedding request has a content length")];
    stream.read_exact(&mut body).await?;
    let body: Value = serde_json::from_slice(&body).unwrap();
    let input: Vec<&str> = match &body["input"] {
        Value::String(text) => vec![text],
        Value::Array(items) => items.iter().map(|item| item.as_str().unwrap()).collect(),
        other => panic!("unexpected embedding input: {other}"),
    };
    calls.embeds.fetch_add(1, Ordering::Relaxed);
    if input.iter().any(|text| text.contains(QUESTION)) {
        calls.queries.fetch_add(1, Ordering::Relaxed);
    }
    let data = (0..input.len())
        .map(|index| json!({"index": index, "embedding": [0.1, 0.2, 0.3]}))
        .collect::<Vec<_>>();
    let body = json!({"data": data}).to_string();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.get_mut().write_all(header.as_bytes()).await?;
    stream.get_mut().write_all(body.as_bytes()).await
}

fn configure(home: &Path, project: &Path, base_url: &str, worker: bool) {
    let path = home.join("config/cortexkit/subc.jsonc");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let flag = if worker {
        "AFT_TEST_SCOPE_WORKER"
    } else {
        "AFT_TEST_SCOPE_STATUS_OBSERVER"
    };
    config["modules"]["aft-test-scope-owner"]["env"][flag] = json!("1");
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let config = json!({
        "indexes": {"trigram": false, "callgraph": false, "semantic": true},
        "semantic": {"backend": "openai_compatible", "model": "fixture", "base_url": base_url}
    });
    std::fs::write(home.join("config/cortexkit/aft.jsonc"), config.to_string()).unwrap();
    std::fs::remove_file(project.join(".cortexkit/aft.jsonc")).unwrap();
    std::fs::remove_file(project.join("seed.txt")).unwrap();
    std::fs::create_dir(project.join("src")).unwrap();
    std::fs::write(
        project.join("src/lib.rs"),
        "/// Select a touchdown zone from the terrain survey.\npub fn select_zone() {}\n",
    )
    .unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "src/lib.rs"],
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "seed",
        ],
    ] {
        let output = Command::new("git")
            .current_dir(project)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }
}
