use super::helpers::AftProcess;
use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use aft::url_fetch::{
    cache_content_path_for_url, cache_meta_path_for_url, fetch_url_to_cache,
    is_private_ip_for_test, UrlFetchOptions,
};
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct MockServer {
    addr: SocketAddr,
    _join: thread::JoinHandle<()>,
}

impl MockServer {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }
}

fn spawn_mock_server<F>(max_requests: usize, handler: F) -> MockServer
where
    F: Fn(String, &mut TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let handler = Arc::new(handler);
    let join = thread::spawn(move || {
        for stream in listener.incoming().take(max_requests).flatten() {
            let handler = Arc::clone(&handler);
            let mut stream = stream;
            let path = read_request_path(&mut stream);
            handler(path, &mut stream);
        }
    });
    MockServer { addr, _join: join }
}

fn read_request_path(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut request = Vec::new();
    let mut buf = [0u8; 512];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let n = stream.read(&mut buf).expect("read request");
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&request);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string()
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .expect("write response headers");
    stream.write_all(body).expect("write response body");
    stream.flush().expect("flush response");
}

fn configure_with_storage(project: &Path, storage: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    // Loopback mock servers require trusted configuration permission;
    // individual requests can still disable it.
    let resp = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": [super::helpers::user_config_tier(json!({"url_fetch_allow_private": true}))],
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");
    aft
}

fn translated_zoom(
    aft: &mut AftProcess,
    project: &Path,
    args: serde_json::Value,
) -> serde_json::Value {
    let translated = aft::subc_translate::subc_translate("zoom", &args, project).unwrap();
    let mut request = translated.args;
    request.insert("id".into(), json!("translated-zoom"));
    request.insert("command".into(), json!(translated.command));
    // Enable private-address access for this test request so it can reach
    // the loopback fixture server.
    request.insert("allow_private".into(), json!(true));
    aft.send(&serde_json::Value::Object(request).to_string())
}

fn translated_outline(
    aft: &mut AftProcess,
    project: &Path,
    target: serde_json::Value,
) -> serde_json::Value {
    let translated =
        aft::subc_translate::subc_translate("outline", &json!({"target": target}), project)
            .unwrap();
    let mut request = translated.args;
    request.insert("id".into(), json!("translated-outline"));
    request.insert("command".into(), json!(translated.command));
    request.insert("allow_private".into(), json!(true));
    aft.send(&serde_json::Value::Object(request).to_string())
}

#[test]
fn outline_url_array_fetches_each_html_target_like_a_single_url() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(2, |path, stream| {
        let body = match path.as_str() {
            "/docs/first.html" => {
                b"<html><body><h1>First document</h1><h2>First section</h2></body></html>"
                    .as_slice()
            }
            "/docs/second.html" => {
                b"<html><body><h1>Second document</h1><h2>Second section</h2></body></html>"
                    .as_slice()
            }
            _ => panic!("unexpected URL: {path}"),
        };
        write_response(stream, "200 OK", "text/html", body);
    });
    let first = server.url("/docs/first.html");
    let second = server.url("/docs/second.html");
    let mut aft = configure_with_storage(project.path(), storage.path());
    let response = translated_outline(&mut aft, project.path(), json!([first, second]));
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], true, "{response:#}");
    assert_eq!(response["files_shown"], 2, "{response:#}");
    assert_eq!(response["skipped_files"], json!([]), "{response:#}");
    assert_eq!(response["content_files"], json!([]), "{response:#}");
    let text = response["text"].as_str().unwrap();
    for (url, heading) in [(&first, "First section"), (&second, "Second section")] {
        assert!(text.contains(url), "{text}");
        assert!(text.contains(heading), "{text}");
        let single = translated_outline(&mut aft, project.path(), json!(url));
        assert_eq!(single["success"], true, "{single:#}");
        assert!(
            text.contains(single["text"].as_str().unwrap()),
            "{response:#}"
        );
    }
    assert!(!text.contains(storage.path().to_str().unwrap()), "{text}");
    assert!(aft.shutdown().success());
}

#[test]
fn outline_url_array_can_mix_a_json_url_and_a_local_file() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let file = project.path().join("module.ts");
    fs::write(&file, "export function localFunction() {}\n").unwrap();
    let server = spawn_mock_server(1, |path, stream| {
        assert_eq!(path, "/data.json");
        write_response(
            stream,
            "200 OK",
            "application/json",
            br#"{"remote_value": 42}"#,
        );
    });
    let url = server.url("/data.json");
    let mut aft = configure_with_storage(project.path(), storage.path());
    let response = translated_outline(&mut aft, project.path(), json!([url, "module.ts"]));
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], true, "{response:#}");
    assert_eq!(response["files_shown"], 2, "{response:#}");
    assert_eq!(response["skipped_files"], json!([]), "{response:#}");
    assert_eq!(response["content_files"], json!([file]), "{response:#}");
    let text = response["text"].as_str().unwrap();
    for expected in [url.as_str(), "remote_value", "module.ts", "localFunction"] {
        assert!(text.contains(expected), "{text}");
    }
    let single = translated_outline(&mut aft, project.path(), json!(url));
    assert_eq!(single["success"], true, "{single:#}");
    assert!(
        text.contains(single["text"].as_str().unwrap()),
        "{response:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn outline_url_array_skips_unfetchable_url_with_its_fetch_reason() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let file = project.path().join("kept.ts");
    fs::write(&file, "export function kept() {}\n").unwrap();
    let server = spawn_mock_server(2, |path, stream| {
        assert_eq!(path, "/missing.md");
        write_response(stream, "404 Not Found", "text/plain", b"missing");
    });
    let url = server.url("/missing.md");
    let mut aft = configure_with_storage(project.path(), storage.path());
    let response = translated_outline(&mut aft, project.path(), json!([url, "kept.ts"]));
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], false, "{response:#}");
    assert_eq!(response["files_shown"], 1, "{response:#}");
    assert_eq!(response["content_files"], json!([file]), "{response:#}");
    let skipped = response["skipped_files"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{response:#}");
    assert_eq!(skipped[0]["file"], url, "{response:#}");
    let reason = skipped[0]["reason"].as_str().unwrap();
    let fetch_message = format!("HTTP 404 Not Found fetching {url}");
    assert_eq!(reason, format!("url_fetch_failed: {fetch_message}"));
    let single = translated_outline(&mut aft, project.path(), json!(url));
    assert_eq!(single["success"], false, "{single:#}");
    assert!(
        reason.contains(single["code"].as_str().unwrap()),
        "{response:#}"
    );
    assert!(
        single["message"]
            .as_str()
            .unwrap()
            .starts_with(&fetch_message),
        "{single:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn translated_zoom_http_path_fetches_local_heading() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |path, stream| {
        assert_eq!(path, "/docs/extension-authoring");
        write_response(
            stream,
            "200 OK",
            "text/markdown",
            b"# Extension authoring\n\n## Public extension interfaces\n\nAn extension exports its public API.\n",
        );
    });
    let url = server.url("/docs/extension-authoring");
    let mut aft = configure_with_storage(project.path(), storage.path());
    for key in ["target", "path", "filePath", "url"] {
        let response = translated_zoom(
            &mut aft,
            project.path(),
            json!({ key: url, "symbols": "Public extension interfaces" }),
        );
        assert_eq!(response["success"], true, "{key}: {response:#}");
        assert!(
            response["targets"][0]["response"]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("An extension exports its public API."),
            "{key}: {response:#}"
        );
    }
    assert!(cache_content_path_for_url(storage.path(), &url, ".md").exists());
    assert!(aft.shutdown().success());
}

#[test]
fn translated_zoom_http_targets_fetch_local_heading() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    fs::write(
        project.path().join("http_client.rs"),
        "pub fn client() {}\n",
    )
    .unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "text/markdown",
            b"# Guide\n\n## Public extension interfaces\n\nThe public API is documented here.\n",
        );
    });
    let url = server.url("/guide.md");
    let mut aft = configure_with_storage(project.path(), storage.path());
    let response = translated_zoom(
        &mut aft,
        project.path(),
        json!({
            "path": url,
            "url": url,
            "symbols": "Public extension interfaces",
            "batch": [
                { "target": url, "symbols": "Public extension interfaces" },
                { "target": "http_client.rs", "symbols": "client" }
            ],
            "targets": [
                { "path": url, "symbol": "Public extension interfaces" },
                { "filePath": url, "url": url, "symbol": "Public extension interfaces" },
                { "path": "http_client.rs", "symbol": "client" }
            ]
        }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    let targets = response["targets"].as_array().expect("batch results");
    assert!(
        targets
            .iter()
            .all(|target| target["response"]["success"] == true),
        "{response:#}"
    );
    assert_eq!(targets.len(), 6, "{response:#}");
    for index in [0, 1, 3, 4] {
        let target = &targets[index];
        assert!(
            target["response"]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("The public API is documented here."),
            "{response:#}"
        );
    }
    assert!(
        targets[5]["response"]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("pub fn client()"),
        "{response:#}"
    );
    assert!(aft.shutdown().success());
}

const DOCUSAURUS_HTML: &str = r##"<!doctype html>
<html>
<head>
  <title>Oh My Pi: Hindsight Memory</title>
  <style>.navbar{display:flex}</style>
  <script>window.__docusaurus = {};</script>
</head>
<body>
  <nav class="navbar navbar--fixed-top"><a href="/">Home</a></nav>
  <aside class="theme-doc-toc-desktop">The Retain Pipeline</aside>
  <main>
    <article>
      <h1>Oh My Pi: Hindsight Memory<a class="hash-link" href="#oh-my-pi" aria-label="Direct link to Oh My Pi">&#8203;</a></h1>
      <p>Hindsight Memory helps agents keep useful context.</p>
      <h2 id="motivating-example">Motivating Example<a class="hash-link" href="#motivating-example" aria-label="Direct link to Motivating Example">&#8203;</a></h2>
      <p>A short example introduces the problem.</p>
      <h2 id="index-pipeline">The Index Pipeline<a class="hash-link" href="#index-pipeline" aria-label="Direct link to The Index Pipeline">&#8203;</a></h2>
      <p>The index stage stores observations.</p>
      <h2 id="retrieve-pipeline">The Retrieve Pipeline<a class="hash-link" href="#retrieve-pipeline" aria-label="Direct link to The Retrieve Pipeline">&#8203;</a></h2>
      <p>The retrieve stage finds useful memories.</p>
      <h2 id="retain-pipeline">The Retain Pipeline<a class="hash-link" href="#retain-pipeline" aria-label="Direct link to The Retain Pipeline">&#8203;</a></h2>
      <p>The retain stage trims old memories while preserving the important facts.</p>
      <div class="language-ts codeBlockContainer_mQmQ" style="--prism-color:#F8F8F2">
        <pre class="prism-code language-ts" style="color:#f8f8f2"><code class="codeBlockLines_AclH"><span class="token-line"><span class="token keyword">const</span><span class="token plain"> retain = async (items: MemoryItem[]) =&gt; {</span></span><span class="token-line"><span class="token plain">  return items.filter((item) =&gt; item.score &gt; 0.7);</span></span><span class="token-line"><span class="token plain">};</span></span></code></pre>
      </div>
      <h2 id="benchmarks">Benchmarks<a class="hash-link" href="#benchmarks" aria-label="Direct link to Benchmarks">&#8203;</a></h2>
      <p>Benchmarks compare retention quality.</p>
      <h2 id="what-comes-next">What Comes Next<a class="hash-link" href="#what-comes-next" aria-label="Direct link to What Comes Next">&#8203;</a></h2>
      <p>Future work improves recall.</p>
      <h2 id="conclusion">Conclusion<a class="hash-link" href="#conclusion" aria-label="Direct link to Conclusion">&#8203;</a></h2>
      <p>Clean markdown keeps the article readable.</p>
    </article>
  </main>
  <footer>Footer chrome</footer>
</body>
</html>
"##;

#[test]
fn private_ip_blocked_at_fetch_time() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "private",
            "command": "outline",
            "file": "http://127.0.0.1/foo",
            "allow_private": false,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "private URL should fail: {resp:?}");
    assert!(
        resp["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Blocked private URL host"),
        "unexpected error: {resp:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn cache_hit_revalidates_ssrf() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "text/markdown", b"# Cached\n");
    });
    let url = server.url("/doc.md");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let first = aft.send(
        &json!({
            "id": "prime",
            "command": "outline",
            "file": url,
            "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(first["success"], true, "prime should succeed: {first:?}");

    let second = aft.send(
        &json!({
            "id": "revalidate",
            "command": "outline",
            "file": url,
            "allow_private": false,
        })
        .to_string(),
    );
    assert_eq!(
        second["success"], false,
        "cache hit must revalidate SSRF: {second:?}"
    );
    assert!(second["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Blocked private URL host"));
    assert!(aft.shutdown().success());
}

#[test]
fn body_read_stall_aborts_within_timeout() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: 5\r\nconnection: keep-alive\r\n\r\n"
        )
        .expect("write stall headers");
        stream.flush().expect("flush stall headers");
        // Must dwarf the assert bound below so a broken stall-abort cannot
        // sneak under it; the thread detaches on MockServer drop, so the
        // long sleep never blocks test teardown.
        thread::sleep(Duration::from_secs(120));
    });
    let mut aft = configure_with_storage(project.path(), storage.path());
    let started = Instant::now();

    let resp = aft.send(
        &json!({
            "id": "stall",
            "command": "outline",
            "file": server.url("/stall.md"),
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "stall should fail: {resp:?}");
    // The stall now ends at the fetch deadline (the call budget less a reply
    // margin) or at the per-chunk stall limit, whichever comes first; both
    // are reported as a body-phase timeout.
    assert_eq!(resp["code"], "url_fetch_timeout", "{resp:?}");
    assert_eq!(resp["phase"], "body", "{resp:?}");
    // The bound checks that the abort FIRED (vs riding out the mock's 120s
    // stall), not its precision — a contended Windows CI runner adds tens of
    // seconds of scheduling overhead on top of the timer (memory 6987), which
    // flaked the old 22s bound.
    assert!(
        started.elapsed() < Duration::from_secs(75),
        "stall timeout took too long: {:?}",
        started.elapsed()
    );
    assert!(aft.shutdown().success());
}

#[test]
fn redirect_revalidates_each_hop() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, move |_path, stream| {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{}/private\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            stream.local_addr().unwrap().port()
        )
        .expect("write redirect");
        stream.flush().expect("flush redirect");
    });
    let url = "http://public.test/start";

    let err = fetch_url_to_cache(
        url,
        storage.path(),
        UrlFetchOptions {
            public_host_overrides: vec![(
                "public.test".to_string(),
                vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            )],
            connect_overrides: vec![("public.test".to_string(), server.addr)],
            ..UrlFetchOptions::default()
        },
    )
    .expect_err("redirect to private URL must fail");

    assert!(
        err.to_string().contains("Blocked private URL host"),
        "unexpected error: {err}"
    );
}

#[test]
fn github_blob_url_fetches_from_raw_content_path() {
    let storage = TempDir::new().unwrap();
    let requested_path = Arc::new(Mutex::new(None));
    let requested_path_for_handler = Arc::clone(&requested_path);
    let server = spawn_mock_server(1, move |path, stream| {
        *requested_path_for_handler.lock().unwrap() = Some(path);
        write_response(stream, "200 OK", "text/markdown", b"# Raw GitHub content\n");
    });

    let cached = fetch_url_to_cache(
        "http://www.github.com/owner/repo/blob/main/docs/guide.md?download=1#L42",
        storage.path(),
        UrlFetchOptions {
            public_host_overrides: vec![(
                "raw.githubusercontent.com".to_string(),
                vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            )],
            connect_overrides: vec![("raw.githubusercontent.com".to_string(), server.addr)],
            ..UrlFetchOptions::default()
        },
    )
    .expect("GitHub blob URL should fetch raw content");

    assert_eq!(
        *requested_path.lock().unwrap(),
        Some("/owner/repo/main/docs/guide.md?download=1".to_string())
    );
    assert_eq!(
        fs::read_to_string(cached).unwrap(),
        "# Raw GitHub content\n"
    );
}

#[test]
fn unsupported_content_type_rejected() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "application/pdf", b"%PDF");
    });
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "pdf",
            "command": "outline",
            "file": server.url("/file.pdf"),
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], false, "PDF should fail: {resp:?}");
    let message = resp["message"].as_str().unwrap_or_default();
    assert!(message.contains("Unsupported content type"), "{message}");
    assert!(message.contains("Supported:"), "{message}");
    assert!(aft.shutdown().success());
}

#[test]
fn html_url_is_converted_to_markdown_before_cache_and_zoom_heading_is_clean() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            DOCUSAURUS_HTML.as_bytes(),
        );
    });
    let url = server.url("/blog/oh-my-pi");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let outline = aft.send(
        &json!({
            "id": "html-outline",
            "command": "outline",
            "file": &url,
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        outline["success"], true,
        "HTML outline should succeed after conversion: {outline:?}"
    );
    let text = outline["text"].as_str().expect("outline text");
    // Single-file lines carry the signature directly (no `{vis} {kind}` prefix),
    // so each heading renders as its markdown signature: a leading run of '#'
    // after the indent and any member '.' prefix.
    let heading_count = text
        .lines()
        .filter(|line| line.trim_start().trim_start_matches('.').starts_with('#'))
        .count();
    assert_eq!(
        heading_count, 8,
        "outline should contain exactly article headings: {text}"
    );
    assert!(
        text.contains("The Retain Pipeline"),
        "outline should contain visible heading text: {text}"
    );
    assert!(
        !text.contains('\u{200b}'),
        "outline heading text must not include Docusaurus permalink text: {text:?}"
    );
    assert!(
        !text.contains("hash-link"),
        "outline should be markdown, not raw HTML: {text}"
    );

    let zoom = aft.send(
        &json!({
            "id": "html-zoom",
            "command": "zoom",
            "file": &url,
            "symbol": "The Retain Pipeline",
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        zoom["success"], true,
        "zoom by visible HTML heading should succeed after conversion: {zoom:?}"
    );
    let content = zoom["content"].as_str().expect("zoom content");
    assert!(
        content.contains("## The Retain Pipeline"),
        "zoom should return markdown heading: {content}"
    );
    assert!(
        content.contains("The retain stage trims old memories"),
        "zoom should include prose body: {content}"
    );
    assert!(
        content.contains("```ts"),
        "Docusaurus prism code should become a fenced ts block: {content}"
    );
    assert!(
        content.contains("const retain = async"),
        "zoom should include code body: {content}"
    );
    assert!(
        content.contains("return items.filter"),
        "zoom should preserve Docusaurus token-line code text: {content}"
    );
    assert!(
        !content.contains("<div") && !content.contains("class="),
        "zoom should not expose raw prism/class/style HTML: {content}"
    );

    let markdown_cache = cache_content_path_for_url(storage.path(), &url, ".md");
    let html_cache = cache_content_path_for_url(storage.path(), &url, ".html");
    assert!(
        markdown_cache.exists(),
        "HTML response should be cached as markdown"
    );
    assert!(
        !html_cache.exists(),
        "converted HTML response should not leave a fresh .html cache entry"
    );
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(cache_meta_path_for_url(storage.path(), &url)).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["extension"], ".md");
    assert_eq!(meta["contentType"], "text/markdown; charset=utf-8");

    assert!(aft.shutdown().success());
}

#[test]
fn url_outline_and_zoom_outputs_hide_cache_paths() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "text/markdown; charset=utf-8",
            b"# Cache safety\n\nThe URL is the section handle.\n",
        );
    });
    let url = server.url("/docs/cache-safety.md");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let outline = aft.send(
        &json!({
            "id": "cache-safe-outline",
            "command": "outline",
            "file": &url,
            "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "URL outline failed: {outline:?}");
    let outline_text = outline["text"].as_str().expect("outline text");

    let zoom = aft.send(
        &json!({
            "id": "cache-safe-zoom",
            "command": "zoom",
            "file": &url,
            "symbol": "Cache safety",
            "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "URL zoom failed: {zoom:?}");
    let mut zoom_data = zoom.clone();
    zoom_data.as_object_mut().unwrap().remove("id");
    zoom_data.as_object_mut().unwrap().remove("success");
    let zoom_response = Response::success("cache-safe-zoom", zoom_data);
    let zoom_context = FormatContext::from_tool_call(
        "zoom",
        &json!({"url": &url, "symbol": "Cache safety"}),
        project.path(),
    );
    let zoom_text = format_response_with_context("zoom", &zoom_response, &zoom_context);

    let cache_path = cache_content_path_for_url(storage.path(), &url, ".md");
    let cache_filename = cache_path.file_name().unwrap().to_string_lossy();
    assert_eq!(
        outline_text,
        format!(
            "{url}\nSections are read with aft_zoom using the same url plus a heading.\n\n  # Cache safety 1:3\n"
        )
    );
    assert!(
        outline_text.starts_with(&url),
        "URL must head its outline: {outline_text}"
    );
    assert!(
        outline_text
            .lines()
            .nth(1)
            .is_some_and(|line| line.contains("aft_zoom")
                && line.contains("same url")
                && line.contains("heading")),
        "outline must explain how to read a section: {outline_text}"
    );
    for (label, visible) in [("outline", outline_text), ("zoom", zoom_text.as_str())] {
        assert!(
            !visible.contains("url_cache"),
            "{label} exposed a cache path: {visible}"
        );
        assert!(
            !visible.contains("url-cache"),
            "{label} exposed a cache path: {visible}"
        );
        assert!(
            !visible.contains(cache_filename.as_ref()),
            "{label} exposed a cache filename: {visible}"
        );
        assert!(
            !visible.contains(&cache_path.to_string_lossy().to_string()),
            "{label} exposed a cache path: {visible}"
        );
    }
    assert!(
        zoom_text.starts_with(&url),
        "URL must head its zoom output: {zoom_text}"
    );
    assert!(aft.shutdown().success());
}

const RUSTDOC_HTML: &str = r##"<!doctype html>
<html>
<head><title>stdio</title><script>window.__rustdoc = {};</script></head>
<body>
<main>
<h1>Function <span class="fn">stdio</span>&nbsp;<button id="copy-path" title="Copy item path to clipboard">Copy item path</button></h1>
<p>Creates a stdio transport.</p>
<h2 class="location"><a href="#">stdio</a></h2>
<h2><a class="doc-anchor" href="#stdio-transport">§</a>StdIO Transport</h2>
<p>Transport details.</p>
</main>
</body>
</html>
"##;

#[test]
fn html_url_rustdoc_headings_are_clean_without_chrome() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(2, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            RUSTDOC_HTML.as_bytes(),
        );
    });
    let url = server.url("/rmcp/latest/rmcp/transport/io/fn.stdio.html");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let outline = aft.send(
        &json!({
            "id": "rustdoc-outline",
            "command": "outline",
            "file": &url,
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        outline["success"], true,
        "rustdoc HTML outline should succeed: {outline:?}"
    );
    let text = outline["text"].as_str().expect("outline text");
    assert!(
        text.contains("# Function stdio"),
        "outline should contain clean h1: {text}"
    );
    assert!(
        !text.contains("Copy item path"),
        "outline must not include copy-path button text: {text}"
    );
    assert!(
        text.contains("## stdio"),
        "outline should contain clean self-link h2: {text}"
    );
    assert!(
        !text.contains("[stdio](#)"),
        "outline must not expose self-link as markdown link: {text}"
    );
    assert!(
        text.contains("## StdIO Transport"),
        "outline should contain section title: {text}"
    );
    assert!(
        !text.contains('§'),
        "outline must not include section permalink: {text}"
    );
    assert!(
        !text.contains("](#stdio-transport)"),
        "outline must not include doc-anchor markdown link: {text}"
    );

    let zoom = aft.send(
        &json!({
            "id": "rustdoc-zoom",
            "command": "zoom",
            "file": &url,
            "symbol": "StdIO Transport",
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        zoom["success"], true,
        "zoom by clean rustdoc heading should succeed: {zoom:?}"
    );
    let content = zoom["content"].as_str().expect("zoom content");
    assert!(
        content.contains("## StdIO Transport"),
        "zoom should resolve section heading: {content}"
    );
    assert!(
        content.contains("Transport details."),
        "zoom should include section body: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn html_url_split_code_block_outline_and_zoom_agree() {
    // Trimmed copy of the Chrome DevTools auto-connect page, whose <pre> closes
    // </code> in the middle of a JSON string.
    const FIXTURE: &str = include_str!("../fixtures/url_html/chrome_auto_connect.html");
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(2, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            FIXTURE.as_bytes(),
        );
    });
    let url = server.url("/docs/devtools/agents/use-cases/auto-connect");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let outline = aft.send(
        &json!({
            "id": "split-outline",
            "command": "outline",
            "file": &url,
            "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "outline failed: {outline:?}");
    let text = outline["text"].as_str().expect("outline text");
    let heading_line = text
        .lines()
        .find(|line| line.contains("## Set up auto-connect"))
        .unwrap_or_else(|| panic!("outline should list the section heading: {text}"));
    let (outline_start, outline_end) = heading_line
        .split_whitespace()
        .last()
        .and_then(|range| range.split_once(':'))
        .map(|(start, end)| (start.parse::<u64>().unwrap(), end.parse::<u64>().unwrap()))
        .unwrap_or_else(|| panic!("heading line should end with a line range: {heading_line}"));

    let zoom = aft.send(
        &json!({
            "id": "split-zoom",
            "command": "zoom",
            "file": &url,
            "symbol": "Set up auto-connect",
            "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "zoom failed: {zoom:?}");
    let content = zoom["content"].as_str().expect("zoom content");
    assert_eq!(
        (
            zoom["range"]["start_line"].as_u64(),
            zoom["range"]["end_line"].as_u64()
        ),
        (Some(outline_start), Some(outline_end)),
        "zoom range should match the outline range {outline_start}:{outline_end}: {zoom:?}"
    );

    // Both commands read the same cached Markdown, so the outline's range
    // must select exactly the text zoom returned.
    let markdown =
        fs::read_to_string(cache_content_path_for_url(storage.path(), &url, ".md")).unwrap();
    let lines: Vec<&str> = markdown.lines().collect();
    let sliced = lines[(outline_start as usize - 1)..outline_end as usize].join("\n");
    assert_eq!(
        content, sliced,
        "zoom content vs outline range in:\n{markdown}"
    );

    // The whole JSON sits inside one fence that closes after its last line.
    let args_line = r#""args": ["chrome-devtools-mcp@latest", "--autoConnect"]"#;
    let fence_lines: Vec<usize> = content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("```"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(fence_lines.len(), 2, "one fenced block expected: {content}");
    let body: Vec<&str> = content
        .lines()
        .skip(fence_lines[0] + 1)
        .take(fence_lines[1] - fence_lines[0] - 1)
        .map(str::trim)
        .collect();
    assert_eq!(
        body,
        vec![
            "{",
            r#""mcpServers": {"#,
            r#""chrome-devtools": {"#,
            r#""command": "npx","#,
            args_line,
            "}",
            "}",
            "}",
        ],
        "fenced JSON should be intact: {content}"
    );
    assert!(
        text.contains("## Use cases for auto-connect"),
        "the next section must still be a heading: {text}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn markdown_url_cache_content_is_unchanged() {
    let storage = TempDir::new().unwrap();
    let body = b"# Native Markdown\n\n```rs\nfn main() {}\n```\n";
    let server = spawn_mock_server(1, move |_path, stream| {
        write_response(stream, "200 OK", "text/markdown; charset=utf-8", body);
    });
    let url = server.url("/readme.md");

    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    )
    .expect("markdown fetch should succeed");

    assert_eq!(
        cached,
        cache_content_path_for_url(storage.path(), &url, ".md")
    );
    assert_eq!(fs::read(&cached).unwrap(), body);
    assert!(
        !cache_content_path_for_url(storage.path(), &url, ".html").exists(),
        "native markdown URLs must not be routed through HTML cache"
    );
}

#[test]
fn json_outline_works() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "application/json; charset=utf-8",
            br#"{"name":"aft","version":1,"nested":{"ok":true}}"#,
        );
    });
    let url = server.url("/package.json");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "json",
            "command": "outline",
            "file": url,
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "JSON outline should succeed: {resp:?}"
    );
    assert_eq!(
        resp["kind"], "json_keys",
        "preview metadata must survive outline rendering"
    );
    assert_eq!(
        resp["content_files"],
        json!([]),
        "a URL contributes no local files"
    );
    let text = resp["text"].as_str().expect("outline text");
    assert!(text.contains("name"), "{text}");
    assert!(text.contains("version"), "{text}");
    assert!(text.contains("nested"), "{text}");
    assert!(
        cache_content_path_for_url(storage.path(), &url, ".json").exists(),
        "JSON response should be cached with .json extension"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn body_size_cap() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        let body = format!("# Large\n{}", "x".repeat(11 * 1024 * 1024));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .expect("write oversized headers");
        stream.flush().expect("flush oversized headers");
        let _ = stream.write_all(body.as_bytes());
    });
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "large",
            "command": "outline",
            "file": server.url("/large.md"),
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "bounded outline should succeed: {resp:?}"
    );
    assert_eq!(resp["complete"], false);
    assert!(resp["text"]
        .as_str()
        .unwrap_or_default()
        .contains("Download truncated at 8388608 of"));
    assert!(aft.shutdown().success());
}

// Oversized bodies must remain useful without either downloading or rendering
// the whole document. Put small keys first so the bounded prefix can name them.
fn oversized_json_server() -> MockServer {
    let body = format!(
        "{{\"truncated\":false,\"sha\":\"fixture\",\"tree\":\"{}\"}}",
        "x".repeat(11 * 1024 * 1024)
    );
    spawn_mock_server(1, move |_path, stream| {
        write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).unwrap();
        // The client intentionally closes the connection at its download cap.
        let _ = stream.write_all(body.as_bytes());
    })
}

#[test]
fn url_json_download_and_miss_are_bounded() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = oversized_json_server();
    let url = server.url("/large.json");
    let mut aft = configure_with_storage(project.path(), storage.path());
    for command in ["outline", "zoom"] {
        let resp = aft.send(
            &json!({
                "id": command, "command": command, "url": url,
                "symbols": ["missing"], "allow_private": true,
            })
            .to_string(),
        );
        assert_eq!(resp["success"], true, "bounded document menu: {resp:?}");
        let text = resp["text"]
            .as_str()
            .or_else(|| resp["content"].as_str())
            .unwrap();
        assert!(text.len() <= 50 * 1024, "URL output exceeded 50 KiB");
        let response = aft::protocol::Response::success(command, resp.clone());
        assert!(aft::subc_format::format_response(command, &response, false).len() <= 50 * 1024);
        for key in ["truncated", "sha", "tree"] {
            assert!(text.contains(key), "missing top-level key {key}: {text}");
        }
        assert!(
            text.contains("truncated at 8388608 of"),
            "missing download envelope: {text}"
        );
        assert!(
            text.contains("bytes; narrow with"),
            "missing narrowing advice: {text}"
        );
        if command == "zoom" {
            assert!(text.contains("not found"), "missing symbol miss: {text}");
        }
    }
    let cached = cache_content_path_for_url(storage.path(), &url, ".json");
    assert_eq!(fs::metadata(cached).unwrap().len(), 8 * 1024 * 1024);
    let small = aft.send(
        &json!({"id":"small", "command":"zoom", "url":url,
        "symbols":["truncated"], "allow_private":true})
        .to_string(),
    );
    assert_eq!(small["success"], true);
    assert!(
        small["content"].as_str().unwrap().ends_with("false"),
        "small key value: {small:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn url_json_top_level_value_is_capped() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = oversized_json_server();
    let mut aft = configure_with_storage(project.path(), storage.path());
    let resp = aft.send(
        &json!({
            "id": "value", "command": "zoom", "url": server.url("/large.json"),
            "symbols": ["tree"], "allow_private": true,
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "key selection failed: {resp:?}");
    let text = resp["text"]
        .as_str()
        .or_else(|| resp["content"].as_str())
        .unwrap();
    assert!(text.len() <= 50 * 1024, "URL value output exceeded 50 KiB");
    assert!(
        text.contains(&"x".repeat(100)),
        "selected value was not returned"
    );
    assert!(text.contains("truncated at"), "missing output envelope");
    assert!(text.contains("narrow with"), "missing narrowing advice");
    assert!(aft.shutdown().success());
}

#[test]
fn reply_ceiling_cuts_fake_tool_and_warns() {
    crate::test_helpers::init_test_logger();
    let response = aft::protocol::Response::success(
        "oversized",
        json!({"text": "界".repeat(5 * 1024 * 1024 / 3)}),
    );
    let original = serde_json::to_string(&response).unwrap().len();
    let text = aft::subc_format::format_response("fake_oversized_tool", &response, false);
    assert!(
        text.len() <= 2 * 1024 * 1024,
        "reply ceiling failed to bound fake tool"
    );
    assert!(text.contains(&format!("of {original} bytes by AFT's reply ceiling")));
    assert!(text.ends_with("the tool's own cap failed, please report"));
    let logs = crate::test_helpers::take_logs();
    assert!(
        logs.iter()
            .any(|line| line.contains("fake_oversized_tool") && line.contains("reply ceiling")),
        "missing WARN: {logs:?}"
    );
}

#[test]
fn reply_ceiling_finalization_bounds_late_text() {
    crate::test_helpers::init_test_logger();
    let storage = TempDir::new().unwrap();
    let ctx = aft::context::AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        aft::config::Config {
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    )
    .with_test_checkpoint_store();
    let mut response = aft::protocol::Response::success("late", json!({}));
    let mut text = "界".repeat(5 * 1024 * 1024 / 3);
    aft::response_finalize::finalize_tool_response(
        &mut response,
        &mut text,
        &ctx,
        "session",
        "late_tool",
        false,
    );
    assert!(text.len() <= 2 * 1024 * 1024);
    assert!(text.contains("by AFT's reply ceiling"));
    let mut response =
        aft::protocol::Response::success("direct", json!({"text":"x".repeat(5 * 1024 * 1024)}));
    aft::response_finalize::finalize_response_with_bg_completions(
        &mut response,
        &ctx,
        "session",
        "direct_tool",
        false,
    );
    assert!(response.data["text"].as_str().unwrap().len() <= 2 * 1024 * 1024);
    let logs = crate::test_helpers::take_logs();
    for tool in ["late_tool", "direct_tool"] {
        assert!(
            logs.iter()
                .any(|line| line.contains(tool) && line.contains("reply ceiling")),
            "missing WARN for {tool}: {logs:?}"
        );
    }
}

#[test]
fn url_markdown_rendering_and_heading_misses_are_bounded() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut body = format!("# Large\n{}\n", "界".repeat(100_000));
    for i in 0..1000 {
        body.push_str(&format!("## Heading {i} {}\nbody\n", "label".repeat(20)));
    }
    let server = spawn_mock_server(1, move |_path, stream| {
        write_response(stream, "200 OK", "text/markdown", body.as_bytes())
    });
    let url = server.url("/large.md");
    let mut aft = configure_with_storage(project.path(), storage.path());
    for (command, symbol) in [("outline", "Large"), ("zoom", "Large"), ("zoom", "absent")] {
        let resp = aft.send(
            &json!({"id":symbol, "command":command, "url":url,
            "symbols":[symbol], "allow_private":true})
            .to_string(),
        );
        assert_eq!(resp["success"], symbol != "absent", "unexpected result");
        let text = resp["text"]
            .as_str()
            .or_else(|| resp["content"].as_str())
            .or_else(|| resp["message"].as_str())
            .unwrap();
        assert!(text.len() <= 50 * 1024);
        assert!(
            text.contains("truncated at"),
            "missing rendered output envelope"
        );
        assert!(text.contains("narrow with"));
        if symbol == "absent" {
            assert!(text.contains("Available headings"));
        }
    }
    assert!(aft.shutdown().success());
}

#[test]
fn url_plain_text_does_not_dump_body_on_heading_miss() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "text/plain",
            "body secret ".repeat(10000).as_bytes(),
        )
    });
    let mut aft = configure_with_storage(project.path(), storage.path());
    let resp = aft.send(
        &json!({"id":"plain", "command":"zoom", "url":server.url("/plain"),
        "symbols":["missing"], "allow_private":true})
        .to_string(),
    );
    assert_eq!(resp["success"], false);
    let text = resp["message"].as_str().unwrap();
    assert!(text.contains("No headings available"));
    assert!(!text.contains("body secret"));
    assert!(text.len() <= 50 * 1024);
    assert!(aft.shutdown().success());
}

#[test]
fn url_unknown_download_size_and_split_utf8_are_disclosed() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        let body = format!("{{\"tree\":\"{}\"}}", "界".repeat(4 * 1024 * 1024));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n"
        )
        .unwrap();
        let _ = stream.write_all(body.as_bytes());
    });
    let url = server.url("/unknown.json");
    let mut aft = configure_with_storage(project.path(), storage.path());
    let resp = aft.send(
        &json!({"id":"unknown", "command":"outline", "url":url, "allow_private":true}).to_string(),
    );
    assert_eq!(
        resp["success"], true,
        "split UTF-8 must remain readable: {resp:?}"
    );
    let text = resp["text"].as_str().unwrap();
    assert!(text.contains("truncated at 8388608 of ≥8388609 bytes"));
    assert!(text.contains("tree"));
    let cached = fs::read(cache_content_path_for_url(storage.path(), &url, ".json")).unwrap();
    assert!(cached.len() <= 8 * 1024 * 1024);
    assert!(std::str::from_utf8(&cached).is_ok());
    assert!(aft.shutdown().success());
}

#[test]
fn cache_writes_atomically() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "application/json", br#"{"atomic":true}"#);
    });
    let url = server.url("/atomic.json");
    let expected_final = cache_content_path_for_url(storage.path(), &url, ".json");
    let saw_tmp_before_rename = Arc::new(AtomicBool::new(false));
    let saw_tmp_before_rename_for_hook = Arc::clone(&saw_tmp_before_rename);
    let expected_final_for_hook = expected_final.clone();

    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            atomic_write_observer: Some(Arc::new(move |tmp, final_path| {
                if final_path == expected_final_for_hook.as_path() {
                    let name = tmp
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    assert!(
                        name.contains(".tmp-"),
                        "temp name should contain .tmp-: {name}"
                    );
                    assert!(tmp.exists(), "temp file should exist before rename");
                    assert!(
                        !final_path.exists(),
                        "final path must not be visible before atomic rename"
                    );
                    saw_tmp_before_rename_for_hook.store(true, Ordering::SeqCst);
                }
            })),
            ..UrlFetchOptions::default()
        },
    )
    .expect("fetch should succeed");

    assert_eq!(cached, expected_final);
    assert!(saw_tmp_before_rename.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(expected_final).unwrap(),
        r#"{"atomic":true}"#
    );
}

#[test]
fn ipv4_mapped_ipv6_ssrf_blocked() {
    for ip in [
        "::ffff:127.0.0.1",
        "::127.0.0.1",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd00::1",
        "ff00::1",
    ] {
        let parsed = ip.parse::<IpAddr>().expect("parse IPv6 test address");
        assert!(is_private_ip_for_test(parsed), "{ip} should be private");
    }

    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut aft = configure_with_storage(project.path(), storage.path());
    for host in ["[::ffff:127.0.0.1]", "[::1]", "[fe80::1]", "[fc00::1]"] {
        let resp = aft.send(
            &json!({
                "id": format!("block-{host}"),
                "command": "outline",
                "file": format!("http://{host}/doc.md"),
                "allow_private": false,
            })
            .to_string(),
        );
        assert_eq!(resp["success"], false, "{host} should fail: {resp:?}");
        assert!(
            resp["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Blocked private URL host"),
            "unexpected error for {host}: {resp:?}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn rfc6598_and_rfc2544_ranges_blocked() {
    // RFC 6598 Shared Address Space (CGNAT) 100.64.0.0/10 and RFC 2544
    // benchmark subnet 198.18.0.0/15 are non-routable and must be treated as
    // private to block SSRF, including via IPv4-mapped IPv6.
    for ip in [
        "100.64.0.1",
        "100.64.1.1",
        "100.100.50.1",
        "100.127.255.255",
        "198.18.0.1",
        "198.19.255.255",
        "::ffff:100.64.0.1",
        "::ffff:198.18.0.1",
    ] {
        let parsed = ip.parse::<IpAddr>().expect("parse test address");
        assert!(is_private_ip_for_test(parsed), "{ip} should be private");
    }

    // Boundary check: addresses just outside the reserved ranges stay public.
    for ip in [
        "100.63.255.255", // just below 100.64.0.0/10
        "100.128.0.1",    // just above 100.127.255.255
        "198.17.255.255", // just below 198.18.0.0/15
        "198.20.0.1",     // just above 198.19.255.255
        "8.8.8.8",        // canonical public
    ] {
        let parsed = ip.parse::<IpAddr>().expect("parse test address");
        assert!(!is_private_ip_for_test(parsed), "{ip} should be public");
    }
}

#[test]
fn concurrent_fetches_do_not_collide() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(2, |_path, stream| {
        write_response(stream, "200 OK", "application/json", br#"{"same":true}"#);
    });
    let url = server.url("/same.json");
    let storage_a = storage.path().to_path_buf();
    let storage_b = storage.path().to_path_buf();
    let url_a = url.clone();
    let url_b = url.clone();

    let first = thread::spawn(move || {
        fetch_url_to_cache(
            &url_a,
            &storage_a,
            UrlFetchOptions {
                allow_private: true,
                ..UrlFetchOptions::default()
            },
        )
    });
    let second = thread::spawn(move || {
        fetch_url_to_cache(
            &url_b,
            &storage_b,
            UrlFetchOptions {
                allow_private: true,
                ..UrlFetchOptions::default()
            },
        )
    });

    let path_a = first.join().unwrap().expect("first fetch");
    let path_b = second.join().unwrap().expect("second fetch");
    assert_eq!(path_a, path_b);
    assert!(cache_content_path_for_url(storage.path(), &url, ".json").exists());
    assert!(cache_meta_path_for_url(storage.path(), &url).exists());

    let cache_dir = storage.path().join("url_cache");
    let entries: Vec<String> = fs::read_dir(cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        entries.iter().filter(|name| name.contains(".tmp-")).count(),
        0,
        "temp files should be cleaned up: {entries:?}"
    );
    assert_eq!(
        entries.len(),
        2,
        "one content file and one meta file should remain: {entries:?}"
    );
}

#[test]
fn transient_connect_failure_retries_then_succeeds() {
    // First connect: TCP listener accepts the connection but drops it without
    // writing any response. reqwest surfaces that as `is_request()` (the body
    // never read a status line), which is_transient_reqwest_error classifies
    // as transient. Second connect: write a real 200 response. Without the
    // retry the outer call would fail; with it the second attempt wins.
    let attempt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempt_for_handler = Arc::clone(&attempt);
    let server = spawn_mock_server(2, move |_path, stream| {
        let n = attempt_for_handler.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Drop without writing anything — peer closed during request.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        } else {
            write_response(stream, "200 OK", "text/markdown", b"# Retried OK\n");
        }
    });
    let url = server.url("/doc.md");
    let storage = TempDir::new().unwrap();

    let start = Instant::now();
    let result = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    );
    let elapsed = start.elapsed();

    let path = result.expect("retry should make the fetch succeed");
    let body = fs::read_to_string(path).unwrap();
    assert!(body.contains("Retried OK"));
    assert_eq!(
        attempt.load(Ordering::SeqCst),
        2,
        "server should see exactly two connect attempts"
    );
    assert!(
        elapsed >= Duration::from_millis(150),
        "first retry should sleep at least one short backoff before reconnecting (elapsed = {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "retry budget shouldn't blow up foreground latency (elapsed = {elapsed:?})"
    );
}

#[test]
fn http_error_status_is_not_retried() {
    // The server *answers* with HTTP 404 on the first request. reqwest treats
    // that as a successful response (status() carries 404), so the caller
    // surfaces "HTTP 404" without re-hammering the server. If the retry loop
    // wrongly retried, the mock would be reached more than once.
    let attempt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempt_for_handler = Arc::clone(&attempt);
    let server = spawn_mock_server(3, move |_path, stream| {
        attempt_for_handler.fetch_add(1, Ordering::SeqCst);
        write_response(stream, "404 Not Found", "text/plain", b"nope\n");
    });
    let url = server.url("/missing.md");
    let storage = TempDir::new().unwrap();

    let result = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    );
    let err = result.expect_err("404 must surface as error");
    assert!(
        err.to_string().contains("HTTP 404"),
        "unexpected error: {err}"
    );
    assert_eq!(
        attempt.load(Ordering::SeqCst),
        1,
        "HTTP error status must NOT trigger a retry"
    );
}

#[test]
fn mjs_url_served_as_javascript_cached_and_outlined_as_js() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let body = br#"export function greet() { return "hi"; }
export class Agent { run() {} }
"#;
    let server = spawn_mock_server(1, move |_path, stream| {
        write_response(stream, "200 OK", "text/javascript", body);
    });
    let url = server.url("/dist/index.mjs");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "mjs-outline",
            "command": "outline",
            "file": url,
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "mjs outline should succeed: {resp:?}"
    );
    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.contains("greet") || text.contains("function"),
        "outline should list JS symbols, not markdown headings: {text}"
    );
    assert!(
        !text.lines().any(|line| line.trim_start().starts_with("# ")),
        "should not be markdown heading outline: {text}"
    );
    assert!(
        cache_content_path_for_url(storage.path(), &url, ".mjs").exists(),
        "mjs URL should cache with .mjs extension"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn rs_url_served_as_plain_text_cached_as_rust() {
    let project = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let body = b"pub fn alpha() {}\npub struct Beta;\n";
    let server = spawn_mock_server(1, move |_path, stream| {
        write_response(stream, "200 OK", "text/plain", body);
    });
    let url = server.url("/crates/aft/src/lib.rs");
    let mut aft = configure_with_storage(project.path(), storage.path());

    let resp = aft.send(
        &json!({
            "id": "rs-outline",
            "command": "outline",
            "file": url,
            "allow_private": true,
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "rust outline should succeed: {resp:?}"
    );
    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.contains("alpha") || text.contains("Beta"),
        "outline should expose Rust symbols: {text}"
    );
    assert!(
        cache_content_path_for_url(storage.path(), &url, ".rs").exists(),
        "plain-text .rs URL must cache as .rs not .md"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn extensionless_javascript_url_falls_back_to_js_extension() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(
            stream,
            "200 OK",
            "application/javascript",
            b"function noop() {}\n",
        );
    });
    let url = server.url("/bundle");
    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    )
    .expect("extensionless JS fetch");
    assert_eq!(
        cached,
        cache_content_path_for_url(storage.path(), &url, ".js")
    );
}

#[test]
fn extensionless_plain_text_still_cached_as_markdown() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "text/plain", b"# Hello\n");
    });
    let url = server.url("/notes");
    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    )
    .expect("plain text fetch");
    assert_eq!(
        cached,
        cache_content_path_for_url(storage.path(), &url, ".md")
    );
}

#[test]
fn stale_wrong_extension_cache_entry_is_refetched() {
    let storage = TempDir::new().unwrap();
    let body = b"pub fn fresh() {}\n";
    let fetch_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fetch_count_handler = Arc::clone(&fetch_count);
    let server = spawn_mock_server(2, move |_path, stream| {
        fetch_count_handler.fetch_add(1, Ordering::SeqCst);
        write_response(stream, "200 OK", "text/plain", body);
    });
    let url = server.url("/lib.rs");

    let meta_path = cache_meta_path_for_url(storage.path(), &url);
    fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let stale_meta = json!({
        "url": url,
        "contentType": "text/plain",
        "extension": ".md",
        "fetchedAt": fetched_at
    });
    fs::write(&meta_path, stale_meta.to_string()).unwrap();
    fs::write(
        cache_content_path_for_url(storage.path(), &url, ".md"),
        b"# stale markdown\n",
    )
    .unwrap();

    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    )
    .expect("should re-fetch after stale extension");
    assert_eq!(
        cached,
        cache_content_path_for_url(storage.path(), &url, ".rs")
    );
    assert_eq!(fs::read(&cached).unwrap(), body);
    assert_eq!(
        fetch_count.load(Ordering::SeqCst),
        1,
        "mock server should be hit once"
    );
}

#[test]
fn nul_bytes_in_source_body_rejected_as_binary() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "text/plain", b"pub fn x() {}\0");
    });
    let url = server.url("/bad.rs");
    let err = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    )
    .expect_err("binary rust body must fail");
    assert!(
        err.to_string().contains("Binary content"),
        "unexpected error: {err}"
    );
}

#[test]
fn url_with_query_resolves_ts_extension() {
    let storage = TempDir::new().unwrap();
    let server = spawn_mock_server(1, |_path, stream| {
        write_response(stream, "200 OK", "text/plain", b"export const x = 1;\n");
    });
    let url = format!("{}?v=2#L10", server.url("/src/mod.ts"));
    let cached = fetch_url_to_cache(
        &url,
        storage.path(),
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        },
    )
    .expect("ts with query fetch");
    assert_eq!(
        cached,
        cache_content_path_for_url(storage.path(), &url, ".ts")
    );
}

#[test]
fn translated_zoom_extensionless_urls_preserve_comma_headings() {
    for (content_type, body) in [
        (
            "text/markdown",
            "# Alpha, beta\n\nFirst section.\n\n# Gamma, delta\n\nSecond section.\n",
        ),
        (
            "text/html",
            "<h1>Alpha, beta</h1><p>First section.</p><h1>Gamma, delta</h1><p>Second section.</p>",
        ),
    ] {
        let project = TempDir::new().unwrap();
        let storage = TempDir::new().unwrap();
        let server = spawn_mock_server(1, move |_path, stream| {
            write_response(stream, "200 OK", content_type, body.as_bytes());
        });
        let url = server.url("/guide");
        let mut aft = configure_with_storage(project.path(), storage.path());
        for (symbols, names) in [
            (json!("Alpha, beta"), vec!["Alpha, beta"]),
            (
                json!(["Alpha, beta", "Gamma, delta"]),
                vec!["Alpha, beta", "Gamma, delta"],
            ),
            (
                json!(r#"["Alpha, beta", "Gamma, delta"]"#),
                vec!["Alpha, beta", "Gamma, delta"],
            ),
        ] {
            let response = translated_zoom(
                &mut aft,
                project.path(),
                json!({ "batch": [{ "target": url, "symbols": symbols }] }),
            );
            assert_eq!(response["success"], true, "{response:#}");
            let entries = response["targets"].as_array().expect("zoom target results");
            assert_eq!(entries.len(), names.len(), "{response:#}");
            for (entry, name) in entries.iter().zip(names) {
                assert_eq!(entry["name"], name, "{response:#}");
                assert_eq!(entry["response"]["success"], true, "{response:#}");
            }
        }
        assert!(aft.shutdown().success());
    }
}
