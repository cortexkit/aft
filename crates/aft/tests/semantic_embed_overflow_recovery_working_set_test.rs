//! What the embed loop retains when the backend *refuses* rows.
//!
//! The sibling test `semantic_embed_working_set_test.rs` measures the loop
//! against a backend that accepts everything, which is the only shape every
//! reproduction attempt for issue #327 had ever run. The tree that reports the
//! problem does not get that backend: it is Cyrillic, dense XML and base64, and
//! its llama.cpp server answers `exceed_context_size_error` for rows the
//! character-based size estimate believed would fit. That reply drives a
//! recovery path added for issue #318 — the failing batch is bisected
//! recursively, and a row that still does not fit is shrunk toward its
//! signature and retried — and a path that by construction does more work per
//! batch when more rows are oversized is worth a number rather than an
//! argument.
//!
//! So this test runs the same corpus twice against the same stub, changing one
//! thing: whether the stub enforces a context limit. Live heap bytes retained
//! per embedded chunk are counted at the build's peak in both runs, which makes
//! the recovery path's own cost the difference between two measurements rather
//! than an absolute number needing its own baseline.
//!
//! The limit is set below what this corpus produces rather than at a real
//! server's 512, because the question is what the recovery path costs when
//! most batches reach it, not whether this particular synthetic corpus would
//! trip a particular server. The token model charges non-ASCII characters
//! about a token each: an English WordPiece vocabulary has no pieces for
//! Cyrillic and falls back to per-byte ones, which is exactly why a row sized
//! in characters can arrive at the backend at twice its estimated length.
//!
//! Live bytes are counted by a global allocator declared in this test binary,
//! so the measurement is exact and is not perturbed by allocator free-list
//! behaviour or by the operating system's page accounting. The counter exists
//! only inside this test binary; nothing in the product's allocation path
//! changes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use aft::config::{SemanticBackend, SemanticBackendConfig};
use aft::semantic_index::{EmbedTextCaps, EmbeddingModel, SemanticIndex};

const DIMENSION: usize = 384;
/// Files in the synthetic corpus. Kept smaller than the accepting-backend
/// test's because every batch here costs a bisection tree of requests instead
/// of one, and the per-chunk figure stabilises well before this many files.
const CORPUS_FILES: usize = 120;
const SYMBOLS_PER_FILE: usize = 20;
const BATCH_SIZE: usize = 64;

/// Whole-row input budget handed to the chunker, and the value issue #327's
/// reporter runs with. Setting it at all selects the wider caps a remote
/// backend gets — body bounded by byte length, whole row bounded by character
/// count — instead of the legacy MiniLM-era ones, so the rows measured here
/// are as long as the reported build's rather than a third of the size.
const MAX_INPUT_TOKENS: usize = 512;

/// Context limit the rejecting stub enforces, in its own token model. Set below
/// what this corpus produces so that nearly every batch reaches the recovery
/// path; a limit no row crossed would make the comparison vacuous.
const STUB_CONTEXT_LIMIT_TOKENS: usize = 320;

/// Ceiling on live heap bytes retained per embedded chunk at the build's peak,
/// the same bound the accepting-backend test applies. Recovery is transient
/// work — bisected sub-batches and shrunk retries are dropped as the recursion
/// unwinds — so crossing this ceiling would mean the path retains per-row or
/// per-batch state on top of the index.
const MAX_RETAINED_BYTES_PER_CHUNK: usize = 4_096;

/// How much more the recovery path may retain at its peak than the same build
/// against an accepting backend. Recursive bisection holds one sub-batch per
/// level while it descends, so some increase is expected and correct; an
/// accumulator that survived the unwind would not stop at a small factor.
const MAX_RECOVERY_RETENTION_RATIO: f64 = 3.0;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// Every allocation adds its layout size and every deallocation subtracts it, so
// the counter is the live total rather than the cumulative total. Relaxed
// ordering is enough: the counter is read from the same thread that ran the
// build, after that build has joined all of its work.
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

/// An English WordPiece vocabulary covers ASCII at roughly three and a half
/// characters per token and has no pieces at all for Cyrillic, falling back to
/// per-byte ones. Charging non-ASCII characters a token each reproduces the gap
/// between what a character-based estimate predicts and what the backend
/// actually counts.
fn estimate_tokens(text: &str) -> usize {
    let non_ascii = text.chars().filter(|c| !c.is_ascii()).count();
    let ascii = text.chars().count() - non_ascii;
    non_ascii + (ascii as f64 / 3.5).ceil() as usize
}

#[derive(Default)]
struct StubCounters {
    requests: AtomicUsize,
    rejections: AtomicUsize,
    rows: AtomicUsize,
    widest_row_tokens: AtomicUsize,
}

/// A loopback embedding backend that either answers every row with the same
/// vector, or refuses any request carrying a row past `context_limit_tokens`
/// with the 400 body a llama.cpp server sends.
struct StubEmbeddingServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    counters: Arc<StubCounters>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StubEmbeddingServer {
    fn start(context_limit_tokens: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub embedding server");
        listener
            .set_nonblocking(true)
            .expect("stub embedding server nonblocking");
        let address = listener
            .local_addr()
            .expect("stub embedding server address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(StubCounters::default());
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_counters = Arc::clone(&counters);
        let handle = thread::spawn(move || {
            // Connections are served one at a time and closed when the client
            // hangs up, so the server itself retains nothing across requests.
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_connection(
                        stream,
                        &thread_shutdown,
                        &thread_counters,
                        context_limit_tokens,
                    ),
                    Err(_) => thread::sleep(std::time::Duration::from_millis(1)),
                }
            }
        });
        Self {
            base_url: format!("http://{address}"),
            shutdown,
            counters,
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

fn serve_connection(
    mut stream: TcpStream,
    shutdown: &AtomicBool,
    counters: &StubCounters,
    context_limit_tokens: Option<usize>,
) {
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
            let inputs = body["input"].as_array().cloned().unwrap_or_default();
            let rows = inputs.len().max(1);
            let widest = inputs
                .iter()
                .filter_map(|value| value.as_str())
                .map(estimate_tokens)
                .max()
                .unwrap_or(0);
            pending.drain(..header_end + content_length);

            counters.requests.fetch_add(1, Ordering::Relaxed);
            counters.rows.fetch_add(rows, Ordering::Relaxed);
            counters
                .widest_row_tokens
                .fetch_max(widest, Ordering::Relaxed);

            let response = match context_limit_tokens {
                Some(limit) if widest > limit => {
                    counters.rejections.fetch_add(1, Ordering::Relaxed);
                    let payload = serde_json::json!({
                        "error": {
                            "type": "exceed_context_size_error",
                            "message": "input is too large to process",
                            "n_prompt_tokens": widest,
                            "n_ctx": limit,
                        }
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\n\r\n{}",
                        payload.len(),
                        payload,
                    )
                }
                _ => {
                    let vector = vec![0.125f32; DIMENSION];
                    let data = (0..rows)
                        .map(|index| serde_json::json!({"embedding": vector, "index": index}))
                        .collect::<Vec<_>>();
                    let payload = serde_json::json!({ "data": data }).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\n\r\n{}",
                        payload.len(),
                        payload,
                    )
                }
            };

            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
        }
    }
}

/// Cyrillic words drawn from the vocabulary of a business-process tree, which
/// is the kind of corpus the report describes. What matters for the
/// measurement is only that they are two bytes per character in UTF-8.
const RUSSIAN_WORDS: [&str; 16] = [
    "процесс",
    "задача",
    "согласование",
    "исполнитель",
    "регламент",
    "уведомление",
    "подразделение",
    "резолюция",
    "поручение",
    "делегирование",
    "эскалация",
    "справочник",
    "реквизит",
    "маршрут",
    "участник",
    "вложение",
];

fn russian_text(seed: usize, words: usize) -> String {
    (0..words)
        .map(|index| RUSSIAN_WORDS[(seed * 7 + index * 13) % RUSSIAN_WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// A corpus whose embedded rows are dominated by multi-byte text. Each symbol
/// carries more Cyrillic in its body than the chunker will keep, so every row
/// reaches the body cap and the rows are uniform in size — otherwise the two
/// runs would differ in what they embed as well as in how the stub answers.
fn write_corpus(root: &Path) -> Vec<PathBuf> {
    let source_dir = root.join("src");
    std::fs::create_dir_all(&source_dir).expect("create corpus directory");
    let mut files = Vec::with_capacity(CORPUS_FILES);
    for file_index in 0..CORPUS_FILES {
        let mut source = String::new();
        for symbol_index in 0..SYMBOLS_PER_FILE {
            let seed = file_index * 101 + symbol_index;
            let doc = russian_text(seed, 24);
            let mut body = String::new();
            for line in 0..8 {
                body.push_str(&format!(
                    "    out.push_str(\"{}\");\n",
                    russian_text(seed + line * 3, 14)
                ));
            }
            source.push_str(&format!(
                "/// {doc}\n\
                 pub fn symbol_{file_index}_{symbol_index}(input: &str, count: usize) -> String {{\n\
                 \x20   let mut out = String::from(input);\n\
                 {body}\
                 \x20   let _ = count;\n\
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

struct BuildMeasurement {
    peak_retained: usize,
    chunks_at_peak: usize,
    chunks: usize,
    batches: usize,
}

impl BuildMeasurement {
    fn per_chunk(&self) -> usize {
        self.peak_retained / self.chunks_at_peak.max(1)
    }
}

fn run_build(base_url: &str, root: &Path, files: &[PathBuf]) -> BuildMeasurement {
    let config = SemanticBackendConfig {
        backend: SemanticBackend::OpenAiCompatible,
        model: "stub-embedding".to_string(),
        base_url: Some(base_url.to_string()),
        api_key_env: None,
        timeout_ms: 30_000,
        max_batch_size: BATCH_SIZE,
        max_input_tokens: Some(MAX_INPUT_TOKENS),
        max_files: 50_000,
        ..Default::default()
    };
    let caps = EmbedTextCaps::from_config(&config);
    let mut model = EmbeddingModel::from_config(&config).expect("configure stub backend");
    let mut embed = |texts: Vec<String>| model.embed(texts);

    // Sample inside the loop, not after it. `done == 0` is the callback the
    // builder fires once the corpus is parsed and before the first batch, so it
    // is the baseline that excludes the parsed chunks and the HTTP client.
    // Every later callback fires between batches, with that batch's work —
    // including any bisection still unwinding — accounted for.
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

    let index = SemanticIndex::build_with_progress_and_cancellation_caps(
        root,
        files,
        &mut embed,
        BATCH_SIZE,
        caps,
        &mut progress,
        &mut keep_going,
    )
    .expect("stub-backed semantic build");

    let measurement = BuildMeasurement {
        peak_retained: peak_retained.load(Ordering::Relaxed),
        chunks_at_peak: peak_done.load(Ordering::Relaxed),
        chunks: index.len(),
        batches: batches.load(Ordering::Relaxed),
    };
    drop(index);
    measurement
}

#[test]
fn overflow_recovery_does_not_multiply_the_embed_loop_working_set() {
    let temp = tempfile::TempDir::new().expect("corpus directory");
    let root = temp.path().canonicalize().expect("canonical corpus root");
    let files = write_corpus(&root);

    let accepting = StubEmbeddingServer::start(None);
    let accepting_build = run_build(&accepting.base_url, &root, &files);
    let accepting_requests = accepting.counters.requests.load(Ordering::Relaxed);
    let widest_row_tokens = accepting.counters.widest_row_tokens.load(Ordering::Relaxed);
    drop(accepting);

    let rejecting = StubEmbeddingServer::start(Some(STUB_CONTEXT_LIMIT_TOKENS));
    let recovery_build = run_build(&rejecting.base_url, &root, &files);
    let recovery_requests = rejecting.counters.requests.load(Ordering::Relaxed);
    let recovery_rejections = rejecting.counters.rejections.load(Ordering::Relaxed);
    drop(rejecting);

    // Non-vacuity first: a limit no row crossed, or a recovery path that never
    // bisected, would make every number below meaningless while still passing.
    assert!(
        widest_row_tokens > STUB_CONTEXT_LIMIT_TOKENS,
        "the corpus must produce rows past the stub's limit, widest was \
         {widest_row_tokens} tokens against a {STUB_CONTEXT_LIMIT_TOKENS} token limit"
    );
    assert!(
        recovery_rejections > recovery_build.batches,
        "recovery must be reached on more than one batch: {recovery_rejections} rejections \
         over {} batches",
        recovery_build.batches
    );
    assert!(
        recovery_requests > accepting_requests * 2,
        "bisection and shrinking must multiply the request count: {recovery_requests} requests \
         with the limit enforced against {accepting_requests} without it"
    );
    assert_eq!(
        recovery_build.chunks, accepting_build.chunks,
        "both runs must embed the same corpus for the comparison to mean anything"
    );

    let accepting_per_chunk = accepting_build.per_chunk();
    let recovery_per_chunk = recovery_build.per_chunk();
    let ratio = recovery_per_chunk as f64 / accepting_per_chunk.max(1) as f64;

    assert!(
        recovery_per_chunk <= MAX_RETAINED_BYTES_PER_CHUNK,
        "with the backend refusing oversized rows the embed build held \
         {recovery_per_chunk} live bytes per chunk at its peak over {} batches \
         ({recovery_rejections} rejections, {recovery_requests} requests), above the \
         {MAX_RETAINED_BYTES_PER_CHUNK} byte ceiling",
        recovery_build.batches
    );
    assert!(
        ratio <= MAX_RECOVERY_RETENTION_RATIO,
        "overflow recovery retained {ratio:.2}x what the same build retained against an \
         accepting backend ({recovery_per_chunk} vs {accepting_per_chunk} bytes per chunk), \
         above the {MAX_RECOVERY_RETENTION_RATIO}x ceiling: bisection or shrinking is holding \
         state past the batch that produced it"
    );

    println!(
        "accepting: {accepting_per_chunk} B/chunk over {} batches, {accepting_requests} requests\n\
         recovery:  {recovery_per_chunk} B/chunk over {} batches, {recovery_requests} requests, \
         {recovery_rejections} rejections (ratio {ratio:.2}x, widest row {widest_row_tokens} tokens)",
        accepting_build.batches, recovery_build.batches,
    );
}
