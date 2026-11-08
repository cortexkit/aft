//! Measures where a URL outline's time goes, in-process, with no daemon.
//!
//! Starts a local HTTP server that can be slow in one phase, then outlines
//! its pages through `run_tool_call` with a phase trace, as the subc
//! transport does. For each call it prints the execution wall time, the
//! calling thread's CPU time over the same span, and the suffix the slow-call
//! log line would carry (`cmd=outline target=url host=... url_fetch=...`).
//!
//! Scenarios:
//!
//! - `fast`: the server answers at once.
//! - `slow_headers`: the server holds its response headers `--delay-ms`.
//! - `slow_body`: the server sends its headers at once and spreads the body
//!   over `--delay-ms`.
//! - `cached`: the `fast` page again, served from the URL cache.
//! - `concurrent`: `--concurrent` outlines of different `slow_headers` pages
//!   at once, to show whether fetches queue behind each other.
//! - `silent`: the server never answers; the outline must end with a typed
//!   timeout well inside the call's 25 s budget.
//!
//! ```text
//! HOME=<throwaway> XDG_CACHE_HOME=<throwaway> XDG_DATA_HOME=<throwaway> \
//! cargo run -p agent-file-tools --example outline_url_waits -- \
//!     [--delay-ms 6000] [--concurrent 4] [--page-kb 200] \
//!     [--scenarios fast,slow_headers,slow_body,cached,concurrent,silent]
//! ```
//!
//! The URL cache lives in a temporary folder the example creates; nothing is
//! read from or written to the user's AFT storage.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::config::Config;
use aft::context::AppContext;
use aft::protocol::Response;
use aft::run_tool_call::{
    run_tool_call, PhaseTrace, ToolCallContext, ToolCallEgressTiming, ToolCallOutcome,
};
use serde_json::json;

struct Args {
    delay: Duration,
    concurrent: usize,
    page_kb: usize,
    scenarios: Vec<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        delay: Duration::from_millis(6_000),
        concurrent: 4,
        page_kb: 200,
        scenarios: [
            "fast",
            "slow_headers",
            "slow_body",
            "cached",
            "concurrent",
            "silent",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect(),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        let value = raw.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--delay-ms" => args.delay = Duration::from_millis(value.parse().unwrap()),
            "--concurrent" => args.concurrent = value.parse().unwrap(),
            "--page-kb" => args.page_kb = value.parse().unwrap(),
            "--scenarios" => {
                args.scenarios = value.split(',').map(str::to_string).collect();
            }
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

/// An HTML page of about `kb` kilobytes with many sections, so the outline
/// has real conversion and parsing work to do.
fn html_page(kb: usize) -> Vec<u8> {
    let mut page = String::from("<html><head><title>Guide</title></head><body><h1>Guide</h1>");
    let mut section = 0;
    while page.len() < kb * 1024 {
        section += 1;
        page.push_str(&format!(
            "<h2>Section {section}</h2><p>Paragraph {section} explains one setting in some detail, \
             with <a href=\"#s{section}\">a link</a> and <code>inline code</code>.</p>\
             <pre><code>setting_{section} = true\n</code></pre>"
        ));
    }
    page.push_str("</body></html>");
    page.into_bytes()
}

fn read_request_path(stream: &mut TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string()
}

/// Serves `page` on every path. The first path segment picks the behaviour:
/// `/fast/..`, `/slow_headers/..`, `/slow_body/..` or `/silent/..`.
fn serve(page: Arc<Vec<u8>>, delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the page server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let page = Arc::clone(&page);
            std::thread::spawn(move || {
                let mut stream = stream;
                let path = read_request_path(&mut stream);
                let mode = path.split('/').nth(1).unwrap_or("fast").to_string();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    page.len()
                );
                match mode.as_str() {
                    "silent" => std::thread::sleep(Duration::from_secs(60)),
                    "slow_headers" => {
                        std::thread::sleep(delay);
                        let _ = stream.write_all(headers.as_bytes());
                        let _ = stream.write_all(&page);
                    }
                    "slow_body" => {
                        let _ = stream.write_all(headers.as_bytes());
                        let pieces = 20;
                        let piece = page.len().div_ceil(pieces);
                        for chunk in page.chunks(piece) {
                            std::thread::sleep(delay / pieces as u32);
                            if stream.write_all(chunk).is_err() {
                                return;
                            }
                        }
                    }
                    _ => {
                        let _ = stream.write_all(headers.as_bytes());
                        let _ = stream.write_all(&page);
                    }
                }
            });
        }
    });
    port
}

/// One outline as the example saw it.
struct Sample {
    scenario: String,
    exec: Duration,
    exec_cpu: Option<Duration>,
    outcome: String,
    shape: String,
}

/// Outline `url` through `run_tool_call` with a phase trace, the way the
/// subc transport runs an agent's call.
fn outline(ctx: &AppContext, root: &std::path::Path, scenario: &str, url: &str) -> Sample {
    let args = json!({"target": url});
    let format = aft::subc_format::FormatContext::from_tool_call("outline", &args, root);
    let tool_ctx = ToolCallContext {
        project_root: root.to_path_buf(),
        session_id: Some("outline-url-waits".into()),
        request_id: format!("outline-{scenario}"),
        standard_edit_grammar: true,
        ..ToolCallContext::default()
    };
    let outcome = std::sync::Mutex::new(String::new());
    let dispatch = |request: aft::protocol::RawRequest, ctx: &AppContext| -> Response {
        let response = aft::commands::outline::handle_outline(&request, ctx);
        *outcome.lock().unwrap() = if response.success {
            "ok".to_string()
        } else {
            let code = response.data["code"].as_str().unwrap_or("error");
            match response.data["phase"].as_str() {
                Some(phase) => format!("{code}/{phase}"),
                None => code.to_string(),
            }
        };
        response
    };
    let mut trace = PhaseTrace::new(Instant::now());
    trace.mark_executor_submitted();
    trace.mark_job_admitted();
    let ToolCallOutcome::Unary(_) = run_tool_call(
        "outline",
        args,
        &format,
        &tool_ctx,
        ctx,
        &dispatch,
        None,
        Some(&mut trace),
    );
    let now = Instant::now();
    let phases = trace
        .finish(ToolCallEgressTiming {
            enqueued: now,
            dequeued: now,
            write_started: now,
            write_finished: now,
            frame_bytes: 1,
            queue_depth: 0,
            writer_active_at_enqueue: false,
            writer_queue_was_full: false,
            reserve_timeouts: 0,
            prepare_cpu: None,
        })
        .expect("every phase was marked");
    let outcome = outcome.into_inner().unwrap();
    Sample {
        scenario: scenario.to_string(),
        exec: phases.execute,
        exec_cpu: phases.cpu.execute,
        outcome,
        shape: phases.arg_shape.unwrap_or_default(),
    }
}

fn ms(duration: Duration) -> String {
    format!("{:.1}", duration.as_secs_f64() * 1000.0)
}

fn main() {
    let args = parse_args();
    let storage = tempfile::tempdir().expect("temporary URL cache");
    let project = tempfile::tempdir().expect("temporary project root");
    let root = project.path().canonicalize().unwrap();
    let port = serve(Arc::new(html_page(args.page_kb)), args.delay);
    let ctx = Arc::new(AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            url_fetch_allow_private: true,
            ..Config::default()
        },
    ));
    let url = |mode: &str, n: usize| format!("http://127.0.0.1:{port}/{mode}/page-{n}.html");

    let mut samples = Vec::new();
    for scenario in &args.scenarios {
        eprintln!("running {scenario}");
        match scenario.as_str() {
            "fast" | "cached" => samples.push(outline(&ctx, &root, scenario, &url("fast", 0))),
            "slow_headers" | "slow_body" | "silent" => {
                samples.push(outline(&ctx, &root, scenario, &url(scenario, 0)))
            }
            "concurrent" => {
                let started = Instant::now();
                let handles = (0..args.concurrent)
                    .map(|n| {
                        let ctx = Arc::clone(&ctx);
                        let root = root.clone();
                        let url = url("slow_headers", n + 1);
                        std::thread::spawn(move || outline(&ctx, &root, "concurrent", &url))
                    })
                    .collect::<Vec<_>>();
                for handle in handles {
                    samples.push(handle.join().expect("concurrent outline"));
                }
                eprintln!(
                    "concurrent: {} outlines took {} ms together",
                    args.concurrent,
                    ms(started.elapsed())
                );
            }
            other => panic!("unknown scenario {other}"),
        }
    }

    println!(
        "delay {} ms, page {} KiB\n",
        args.delay.as_millis(),
        args.page_kb
    );
    println!("| scenario | exec ms | exec cpu ms | outcome | slow-call line suffix |");
    println!("|---|---|---|---|---|");
    for sample in &samples {
        println!(
            "| {} | {} | {} | {} | `{}` |",
            sample.scenario,
            ms(sample.exec),
            sample.exec_cpu.map_or_else(|| "?".to_string(), ms),
            sample.outcome,
            sample.shape
        );
    }
}
