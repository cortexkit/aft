//! The semantic embed build must retain the index it is building and nothing
//! else. Issue #327 reported daemon memory climbing without bound during full
//! embed builds while batches advanced normally, which is the signature of the
//! loop holding on to per-row or per-batch state instead of dropping it once a
//! batch is folded into the index.
//!
//! This test pins that as a number: live heap bytes retained per embedded
//! chunk, sampled *while the loop runs*. That timing is the whole point. The
//! index under construction is the only thing the loop is entitled to hold, and
//! it is fully built by the last batch, so the peak during the build and the
//! total after it differ by exactly the transient state a leak would hold. A
//! version of this test that sampled after the build returned would miss any
//! accumulator scoped to the build function, which is the reported shape.
//!
//! Live bytes are counted by a global allocator declared in this test binary,
//! so the measurement is exact and is not perturbed by other tests, by the
//! allocator's own free-list behaviour, or by the operating system's page
//! accounting. The counter only exists inside this test binary; nothing in the
//! product's allocation path changes.
//!
//! The test drives the real `openai_compatible` lane against a stub embedding
//! server on loopback, which is the backend family the issue reports.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use aft::config::{SemanticBackend, SemanticBackendConfig};
use aft::semantic_index::{EmbeddingModel, SemanticIndex};

/// Width of each vector the stub returns. 384 is the width of the all-MiniLM
/// family that the default local backend uses, so the per-chunk figures here
/// are comparable to a real build's.
const DIMENSION: usize = 384;
/// Files in the synthetic corpus. Large enough that the per-chunk figure is
/// dominated by the build and not by the daemon's fixed startup cost.
const CORPUS_FILES: usize = 240;
/// Symbols per file. Each one becomes its own embedded chunk.
const SYMBOLS_PER_FILE: usize = 20;
const BATCH_SIZE: usize = 64;

/// Ceiling on live heap bytes retained per embedded chunk at the peak of the
/// build. Everything under it is the index being built: a 384-wide vector is
/// 1536 bytes, and while the loop runs each chunk is held twice — once in the
/// collected corpus it is reading from and once in the entry it just produced.
/// That measures close to 3.1 KB per chunk here.
///
/// Four kilobytes leaves room for roughly one more kilobyte of stored content
/// per chunk before the tripwire fires, which is a real change to the index's
/// shape and should be re-baselined on purpose. The measurement is exact (a
/// counting allocator over a fixed corpus, no operating system page
/// accounting), so it does not drift on its own.
///
/// Note the sensitivity this buys: an accumulator holding less than about a
/// kilobyte per chunk still passes. What it catches is buffering on the order
/// of a vector or a response body per row, which is the reported failure and
/// several hundred times larger than that floor.
const MAX_RETAINED_BYTES_PER_CHUNK: usize = 4_096;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// Every allocation adds its layout size and every deallocation subtracts it, so
// the counter is the live total rather than the cumulative total. Relaxed
// ordering is enough: the test reads the counter from the same thread that ran
// the build, after the build has joined all of its work.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(pointer, layout);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = System.realloc(pointer, layout, new_size);
        if !new_pointer.is_null() {
            LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// A loopback embedding backend that answers every row with the same vector.
/// Using a stub rather than a real backend keeps the measurement about AFT's
/// own retention and lets the test drive thousands of rows in under a second.
struct StubEmbeddingServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StubEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub embedding server");
        listener
            .set_nonblocking(true)
            .expect("stub embedding server nonblocking");
        let address = listener
            .local_addr()
            .expect("stub embedding server address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            // Connections are served one at a time and closed when the client
            // hangs up, so the server itself retains nothing across requests.
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_connection(stream, &thread_shutdown),
                    Err(_) => thread::sleep(std::time::Duration::from_millis(1)),
                }
            }
        });
        Self {
            base_url: format!("http://{address}"),
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for StubEmbeddingServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(mut stream: TcpStream, shutdown: &AtomicBool) {
    stream.set_nonblocking(false).ok();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let mut pending = Vec::new();
    let mut chunk = [0u8; 8192];
    while !shutdown.load(Ordering::SeqCst) {
        let count = match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(count) => count,
            Err(_) => return,
        };
        pending.extend_from_slice(&chunk[..count]);
        while let Some(position) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = position + 4;
            let mut content_length = 0usize;
            for line in String::from_utf8_lossy(&pending[..header_end]).lines() {
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    content_length = line
                        .split_once(':')
                        .and_then(|(_, value)| value.trim().parse().ok())
                        .unwrap_or(0);
                }
            }
            if pending.len() < header_end + content_length {
                break;
            }
            let body: serde_json::Value =
                serde_json::from_slice(&pending[header_end..header_end + content_length])
                    .expect("stub embedding request body");
            let rows = body["input"]
                .as_array()
                .map(|inputs| inputs.len())
                .unwrap_or(1);
            pending.drain(..header_end + content_length);

            let vector = vec![0.125f32; DIMENSION];
            let data = (0..rows)
                .map(|index| serde_json::json!({"embedding": vector, "index": index}))
                .collect::<Vec<_>>();
            let payload = serde_json::json!({ "data": data }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload,
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
        }
    }
}

fn write_corpus(root: &std::path::Path) -> Vec<PathBuf> {
    let source_dir = root.join("src");
    std::fs::create_dir_all(&source_dir).expect("create corpus directory");
    let mut files = Vec::with_capacity(CORPUS_FILES);
    for file_index in 0..CORPUS_FILES {
        let mut source = String::new();
        for symbol_index in 0..SYMBOLS_PER_FILE {
            source.push_str(&format!(
                "/// Documentation for this symbol, long enough that the embedded row \
                 carries real text rather than just a name.\n\
                 pub fn symbol_{file_index}_{symbol_index}(input: &str, count: usize) -> String {{\n\
                 \x20   let mut out = String::new();\n\
                 \x20   for step in 0..count {{\n\
                 \x20       out.push_str(&format!(\"{{input}}-{{step}}-{symbol_index}\"));\n\
                 \x20   }}\n\
                 \x20   out\n\
                 }}\n\n"
            ));
        }
        let path = source_dir.join(format!("module_{file_index}.rs"));
        std::fs::write(&path, source).expect("write corpus file");
        files.push(path);
    }
    files
}

#[test]
fn embed_build_retains_only_the_index_it_is_building() {
    let server = StubEmbeddingServer::start();
    let temp = tempfile::TempDir::new().expect("corpus directory");
    let root = temp.path().canonicalize().expect("canonical corpus root");
    let files = write_corpus(&root);

    let config = SemanticBackendConfig {
        backend: SemanticBackend::OpenAiCompatible,
        model: "stub-embedding".to_string(),
        base_url: Some(server.base_url.clone()),
        api_key_env: None,
        timeout_ms: 30_000,
        max_batch_size: BATCH_SIZE,
        max_files: 50_000,
        ..Default::default()
    };
    let mut model = EmbeddingModel::from_config(&config).expect("configure stub backend");
    let mut embed = |texts: Vec<String>| model.embed(texts);

    // Sample inside the loop, not after it. `done == 0` is the callback the
    // builder fires once the corpus is parsed and before the first batch, so it
    // is the baseline that excludes the parsed chunks and the HTTP client. Every
    // later callback fires between batches, with that batch's work still live.
    let baseline = AtomicUsize::new(0);
    let peak_retained = AtomicUsize::new(0);
    let peak_done = AtomicUsize::new(0);
    let batches = AtomicUsize::new(0);
    let mut progress = |done: usize, _total: usize| {
        let live = live_bytes();
        if done == 0 {
            baseline.store(live, Ordering::Relaxed);
            return;
        }
        batches.fetch_add(1, Ordering::Relaxed);
        let retained = live.saturating_sub(baseline.load(Ordering::Relaxed));
        if retained > peak_retained.load(Ordering::Relaxed) {
            peak_retained.store(retained, Ordering::Relaxed);
            peak_done.store(done, Ordering::Relaxed);
        }
    };
    let mut keep_going = || true;

    let index = SemanticIndex::build_with_progress_and_cancellation(
        &root,
        &files,
        &mut embed,
        BATCH_SIZE,
        &mut progress,
        &mut keep_going,
    )
    .expect("stub-backed semantic build");

    let retained = peak_retained.load(Ordering::Relaxed);
    let chunks_at_peak = peak_done.load(Ordering::Relaxed);
    let chunks = index.len();
    let observed_batches = batches.load(Ordering::Relaxed);

    assert!(
        chunks >= CORPUS_FILES * SYMBOLS_PER_FILE,
        "the corpus should produce at least one chunk per symbol, got {chunks}"
    );
    assert!(
        observed_batches > 1,
        "the bound is only meaningful across many batches, ran {observed_batches}"
    );

    let per_chunk = retained / chunks_at_peak.max(1);
    assert!(
        per_chunk <= MAX_RETAINED_BYTES_PER_CHUNK,
        "embed build held {per_chunk} live bytes per chunk at its peak over {observed_batches} \
         batches ({retained} bytes with {chunks_at_peak} chunks embedded), above the \
         {MAX_RETAINED_BYTES_PER_CHUNK} byte ceiling: the loop is holding per-row or per-batch \
         state on top of the index it is building"
    );

    drop(index);
}
