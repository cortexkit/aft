//! Measures what `aft_search` calls wait on, in-process, with no daemon.
//!
//! Binds a primary folder (a copy of a real repository) to a fresh store,
//! waits until its text and meaning indexes are ready, then runs concurrent
//! searches through the real search handler in several phases:
//!
//! - `idle`: nothing else runs.
//! - `cold`: a second folder binds cold while the searches run; its build and
//!   then its meaning fill run in the data layer's background pools. Each
//!   search is tagged with the second folder's index state when it started,
//!   so the build and the fill show up apart.
//! - `edits`: a writer edits files of the primary folder through AFT's own
//!   `write` command while the searches run.
//! - `fresh`: a third folder is configured and searched at once, so the
//!   searches meet that root's own bind, build and fill (tagged by stage).
//!
//! Every search runs inside a wait scope (`commands::semantic_search::waits`)
//! that samples thread CPU at each timer, so each named wait splits into on-
//! and off-CPU time. The semantic backend is a local OpenAI-compatible
//! embedding server started by this example (deterministic word-hash vectors,
//! a fixed latency per request), so no model download or ONNX Runtime is
//! needed; `--embed-latency-ms` sets how slow it is.
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example search_waits -- \
//!     --root <primary folder> --cold <second folder> --fresh <third folder> \
//!     --storage <empty dir> \
//!     [--searchers 4] [--phase-secs 20] [--cold-max-secs 240] \
//!     [--embed-latency-ms 10] [--queries <file>] [--phases idle,cold,edits,fresh] \
//!     [--samples <jsonl file>] [--unique 1]
//! ```
//!
//! Every folder should be a plain copy (for example `git archive HEAD | tar
//! -x`). AFT shares one store between all checkouts of a git repository but
//! gives a plain folder a store of its own, so the second and third copies
//! build cold, and edits never touch a real checkout. The query file holds one query
//! per line; a tab separates a query from a pattern for split searches.
//! With `--unique 1` (the default) each prose query gets three extra words
//! chosen by the call's number, so its embedding misses the query cache.
//! Prints a markdown summary to stdout and progress to stderr.
//! Set `AFT_SEARCH_WAITS_RERANK=1` to use the loopback server as a deterministic
//! reranker too. It returns tied scores without a model, isolating candidate
//! preparation from inference. `--warmup 0` includes first-use contention.
//! `--requests N` bounds the idle phase to the same query sequence across builds
//! (the phase deadline still applies).

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft::commands::semantic_search::waits::{self, SearchWait, SearchWaits};
use aft::config::Config;
use aft::context::{default_language_provider_factory, AppContext};
use aft::protocol::RawRequest;
use serde_json::{json, Value};

const DIMENSION: usize = 384;

/// Words appended to prose queries to make each call's text unique.
const VARIANT_WORDS: [&str; 16] = [
    "detail", "handling", "path", "logic", "flow", "usage", "setup", "state", "error", "result",
    "config", "update", "check", "limit", "order", "cache",
];

const DEFAULT_QUERIES: &[&str] = &[
    "pub fn handle_configure",
    "struct CatalogEntry",
    "callgraph_unavailable_aggregate",
    "validate_read_path|restrict_to_project_root",
    "MAX_EMBED_TEXT_CHARS|fastembed",
    "home_root|is_home_root|degraded_reasons",
    "how does apply_patch write files with durability fsync and backup op_id",
    "status snapshot session id tracked_files checkpoints json serialization",
    "set_progress_sender install standalone push frame stdout closure wiring",
    "AFT_LOG_STDERR runtime OpenCode plugin environment",
    "how does ast_grep_search parallelize work across files (existing pattern we can copy)",
    "claim_or_open_read_only(",
    "callgraph_op subc_format.rs",
    "rate-limited warning logging",
    "where is the exact memo single flight\tin_flight_changed",
    "how are query embeddings cached\tembed_query_cached",
];

struct Args {
    root: PathBuf,
    cold: Option<PathBuf>,
    fresh: Option<PathBuf>,
    storage: PathBuf,
    searchers: usize,
    phase: Duration,
    cold_max: Duration,
    embed_latency: Duration,
    queries: Vec<(String, Option<String>)>,
    phases: Vec<String>,
    samples: Option<PathBuf>,
    unique: bool,
    warmup: bool,
    requests: u64,
}

fn parse_args() -> Args {
    let mut values = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let name = flag
            .strip_prefix("--")
            .unwrap_or_else(|| panic!("unexpected argument {flag}"))
            .to_string();
        let value = args
            .next()
            .unwrap_or_else(|| panic!("--{name} needs a value"));
        values.insert(name, value);
    }
    let path = |name: &str| values.get(name).map(PathBuf::from);
    let number = |name: &str, default: u64| {
        values
            .get(name)
            .map_or(default, |value| value.parse().unwrap())
    };
    let queries = match values.get("queries") {
        Some(file) => std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        None => DEFAULT_QUERIES
            .iter()
            .map(|query| query.to_string())
            .collect(),
    }
    .into_iter()
    .filter(|line| !line.trim().is_empty())
    .map(|line| match line.split_once('\t') {
        Some((query, pattern)) => (query.to_string(), Some(pattern.to_string())),
        None => (line, None),
    })
    .collect();
    Args {
        root: std::fs::canonicalize(path("root").expect("--root is required")).unwrap(),
        cold: path("cold").map(|cold| std::fs::canonicalize(cold).unwrap()),
        fresh: path("fresh").map(|fresh| std::fs::canonicalize(fresh).unwrap()),
        storage: path("storage").expect("--storage is required"),
        searchers: number("searchers", 4) as usize,
        phase: Duration::from_secs(number("phase-secs", 20)),
        cold_max: Duration::from_secs(number("cold-max-secs", 240)),
        embed_latency: Duration::from_millis(number("embed-latency-ms", 10)),
        queries,
        phases: values
            .get("phases")
            .map_or("idle,cold,edits", String::as_str)
            .split(',')
            .map(str::to_string)
            .collect(),
        samples: path("samples"),
        unique: number("unique", 1) != 0,
        warmup: number("warmup", 1) != 0,
        requests: number("requests", u64::MAX),
    }
}

/// A word-hash vector: texts sharing words point the same way, so the
/// semantic lane ranks something meaningful without a real model.
fn fake_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIMENSION];
    for word in text
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|word| !word.is_empty())
    {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in word.to_ascii_lowercase().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        vector[(hash % DIMENSION as u64) as usize] += 1.0;
        vector[((hash >> 32) % DIMENSION as u64) as usize] -= 0.5;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm == 0.0 {
        vector[0] = 1.0;
    } else {
        vector.iter_mut().for_each(|value| *value /= norm);
    }
    vector
}

/// Serves `POST .../embeddings` in the OpenAI shape on a loopback port, one
/// thread per connection, sleeping `latency` per request. Returns the base
/// URL and a request counter.
fn serve_embeddings(latency: Duration) -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&requests);
    std::thread::Builder::new()
        .name("fake-embed-accept".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    let _ = answer_embeddings(stream, latency, &counter);
                });
            }
        })
        .unwrap();
    (base, requests)
}

fn answer_embeddings(
    stream: TcpStream,
    latency: Duration,
    counter: &AtomicU64,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    stream.set_nodelay(true)?;
    let mut writer = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        counter.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::from_slice::<Value>(&body).unwrap();
        let inputs = body
            .get("input")
            .cloned()
            .map(|input| match input {
                Value::Array(items) => items
                    .iter()
                    .map(|item| item.as_str().unwrap_or_default().to_string())
                    .collect(),
                Value::String(text) => vec![text],
                _ => Vec::new(),
            })
            .unwrap_or_default();
        if !request_line.contains("/rerank ") {
            std::thread::sleep(latency);
        }
        let data = inputs
            .iter()
            .enumerate()
            .map(|(index, text)| json!({"index": index, "embedding": fake_vector(text)}))
            .collect::<Vec<_>>();
        let reply = if request_line.contains("/rerank ") {
            let results = body["documents"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, _)| json!({"index": index, "relevance_score": 0.5}))
                .collect::<Vec<_>>();
            json!({"results": results}).to_string()
        } else {
            json!({"data": data, "model": "search-waits-fake"}).to_string()
        };
        // One write of the whole response. Writing the header and the body
        // separately left the body waiting for the client's acknowledgement
        // of the header (TCP's Nagle algorithm against delayed ACKs), which
        // added about 40 ms to every round trip on Linux.
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            reply.len(),
            reply
        );
        writer.write_all(response.as_bytes())?;
        writer.flush()?;
    }
}

/// A context bound to `root` through the real `configure` command, its
/// store in `storage`, its semantic search backend set to the fake embedding
/// server, with
/// configure's deferred maintenance drained (which binds the checkout).
fn bind(root: &Path, storage: &Path, embed_base: &str, label: &str) -> Arc<AppContext> {
    let ctx = configure(root, storage, embed_base, label);
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    ctx
}

/// [`bind`] without the drain: the checkout binds on the first search or
/// when the caller drains.
fn configure(root: &Path, storage: &Path, embed_base: &str, label: &str) -> Arc<AppContext> {
    std::fs::create_dir_all(storage).unwrap();
    let ctx = AppContext::new(
        default_language_provider_factory(),
        Config {
            storage_dir: Some(storage.to_path_buf()),
            ..Config::default()
        },
    );
    let mut user = json!({
        "indexes": {"trigram": true, "semantic": true, "callgraph": false},
        "semantic": {
            "backend": "openai_compatible",
            "base_url": embed_base,
            "model": "search-waits-fake",
        },
    });
    if std::env::var_os("AFT_SEARCH_WAITS_RERANK").is_some() {
        user["search"] = json!({"rerank": {
            "backend": "remote", "endpoint": embed_base,
            "model": "search-waits-tied", "timeout_ms": 5000,
        }});
    }
    let configure = request(json!({
        "id": format!("configure-{label}"),
        "command": "configure",
        "session_id": format!("search-waits-{label}"),
        "harness": "opencode",
        "project_root": root.display().to_string(),
        "storage_dir": storage.display().to_string(),
        "config": [{"tier": "user", "doc": user.to_string()}],
    }));
    let response = aft::commands::configure::handle_configure(&configure, &ctx);
    assert!(
        response.success,
        "configure {label} failed: {}",
        serde_json::to_string(&response.data).unwrap()
    );
    Arc::new(ctx)
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).unwrap()
}

/// The text and semantic index statuses health reports for `root`.
fn index_state(ctx: &AppContext, root: &Path) -> (String, String) {
    let health = serde_json::to_value(ctx.try_health_snapshot(root)).unwrap();
    let status = |component: &str| {
        health[component]["status"]
            .as_str()
            .unwrap_or("absent")
            .to_string()
    };
    (status("search_index"), status("semantic_index"))
}

fn wait_ready(ctx: &AppContext, root: &Path, limit: Duration) -> Duration {
    let started = Instant::now();
    let mut last = (String::new(), String::new());
    loop {
        let state = index_state(ctx, root);
        if state.0 == "ready" && state.1 == "ready" {
            return started.elapsed();
        }
        if state != last {
            eprintln!(
                "{}: text={} semantic={} ({:.0} s)",
                root.display(),
                state.0,
                state.1,
                started.elapsed().as_secs_f64()
            );
            last = state;
        }
        assert!(
            started.elapsed() < limit,
            "{} did not become ready: {last:?}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// One search call as the example saw it.
struct Sample {
    phase: String,
    /// What the background work was doing when the call started.
    background: String,
    query: usize,
    success: bool,
    /// The error code of a failed call.
    code: Option<String>,
    waits: SearchWaits,
}

/// Runs `searchers` threads of back-to-back searches against `ctx` until
/// `keep_going` says stop, tagging each call with `background()`.
fn run_searches(
    phase: &str,
    ctx: &AppContext,
    queries: &[(String, Option<String>)],
    searchers: usize,
    unique: bool,
    keep_going: &(dyn Fn() -> bool + Sync),
    background: &(dyn Fn() -> String + Sync),
) -> Vec<Sample> {
    let samples = Mutex::new(Vec::new());
    let next_id = AtomicU64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..searchers {
            let samples = &samples;
            let next_id = &next_id;
            scope.spawn(move || {
                while keep_going() {
                    let id = next_id.fetch_add(1, Ordering::Relaxed);
                    let query = id as usize % queries.len();
                    let (text, pattern) = &queries[query];
                    // Prose queries get three extra words chosen by the call's
                    // number, so their embeddings miss the query cache as
                    // live traffic's mostly unique queries do. Identifier
                    // queries stay as they are: extra words would turn them
                    // into prose and change their whole plan.
                    let text = if unique && text.contains(' ') {
                        format!(
                            "{text} {} {} {}",
                            VARIANT_WORDS[id as usize % 16],
                            VARIANT_WORDS[id as usize / 16 % 16],
                            VARIANT_WORDS[id as usize / 256 % 16]
                        )
                    } else {
                        text.clone()
                    };
                    let mut body = json!({
                        "id": format!("{phase}-{id}"),
                        "command": "semantic_search",
                        "session_id": "search-waits-primary",
                        "query": text,
                        "top_k": 10,
                    });
                    if let Some(pattern) = pattern {
                        body["pattern"] = json!(pattern);
                    }
                    let req = request(body);
                    let tag = background();
                    let scope = waits::begin_with_wait_cpu();
                    let response =
                        aft::commands::semantic_search::handle_semantic_search(&req, ctx);
                    let waits = scope.finish().expect("outermost scope");
                    samples.lock().unwrap().push(Sample {
                        phase: phase.to_string(),
                        background: tag,
                        query,
                        success: response.success,
                        code: response.data["code"].as_str().map(str::to_string),
                        waits,
                    });
                }
            });
        }
    });
    samples.into_inner().unwrap()
}

/// Process CPU (user + system) so far, and the 1-minute load average.
#[cfg(unix)]
fn process_cpu_and_load() -> (f64, f64) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // getrusage fills the struct on success; a failure reports zero.
    let cpu = if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
        let usage = unsafe { usage.assume_init() };
        let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
        seconds(usage.ru_utime) + seconds(usage.ru_stime)
    } else {
        0.0
    };
    let mut load = [0.0f64; 3];
    let loaded = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    (cpu, if loaded > 0 { load[0] } else { -1.0 })
}

#[cfg(not(unix))]
fn process_cpu_and_load() -> (f64, f64) {
    (0.0, -1.0)
}

struct PhaseReport {
    name: String,
    seconds: f64,
    process_cpu_seconds: f64,
    load_start: f64,
    load_end: f64,
    embed_requests: u64,
    /// Bytes the whole process read through `read(2)` during the phase
    /// (`rchar` of /proc/self/io; zero where that file does not exist).
    bytes_read: u64,
    searches: usize,
}

/// Bytes this process has read through `read(2)` and friends so far, from
/// `rchar` in /proc/self/io; zero where that file does not exist.
fn process_bytes_read() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|io| {
            io.lines()
                .find_map(|line| line.strip_prefix("rchar:")?.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// The numbers of one field of the slow-call rendering of `waits`, for
/// example ` files=read:3/1200B,repeat:0,...` gives `[3, 1200, 0, ...]`.
/// Empty when the call did not report the field (or the build predates it),
/// so the same example runs against older builds.
fn rendered_numbers(waits: &SearchWaits, key: &str) -> Vec<u64> {
    let line = waits.render();
    let Some(start) = line.find(key) else {
        return Vec::new();
    };
    line[start + key.len()..]
        .split(' ')
        .next()
        .unwrap_or_default()
        .split(|character: char| !character.is_ascii_digit())
        .filter_map(|number| number.parse().ok())
        .collect()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(sorted: &[f64], share: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * share).round() as usize]
}

fn distribution(mut values: Vec<f64>) -> String {
    values.sort_by(f64::total_cmp);
    format!(
        "{:.1} | {:.1} | {:.1}",
        percentile(&values, 0.5),
        percentile(&values, 0.9),
        percentile(&values, 1.0)
    )
}

/// One markdown table for `samples`: per call wall, CPU and off-CPU time,
/// then each wait kind over the calls where it ran.
fn summarize(title: &str, samples: &[&Sample]) {
    if samples.is_empty() {
        return;
    }
    let mut failed = BTreeMap::<&str, usize>::new();
    for sample in samples.iter().filter(|sample| !sample.success) {
        *failed
            .entry(sample.code.as_deref().unwrap_or("error"))
            .or_default() += 1;
    }
    println!(
        "\n### {title}: {} calls (failed: {failed:?})\n",
        samples.len()
    );
    println!("| measure | calls | p50 ms | p90 ms | max ms | off-CPU p50 ms | off-CPU p90 ms |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    let wall = samples.iter().map(|sample| ms(sample.waits.wall));
    let cpu = samples
        .iter()
        .map(|sample| sample.waits.cpu.map_or(f64::NAN, ms));
    let off = samples.iter().map(|sample| {
        sample
            .waits
            .cpu
            .map_or(f64::NAN, |cpu| ms(sample.waits.wall.saturating_sub(cpu)))
    });
    println!(
        "| call wall | {} | {} | {} |",
        samples.len(),
        distribution(wall.collect()),
        distribution(off.collect())
            .rsplit_once(" | ")
            .map(|(head, _)| head.to_string())
            .unwrap()
    );
    println!(
        "| call thread CPU | {} | {} | | |",
        samples.len(),
        distribution(cpu.collect())
    );
    for kind in SearchWait::ALL {
        let ran = samples
            .iter()
            .map(|sample| sample.waits.slot(kind))
            .filter(|slot| slot.count > 0)
            .collect::<Vec<_>>();
        if ran.is_empty() {
            continue;
        }
        let total = ran.iter().map(|slot| ms(slot.total)).collect::<Vec<_>>();
        let mut off = ran
            .iter()
            .map(|slot| ms(slot.total.saturating_sub(slot.cpu)))
            .collect::<Vec<_>>();
        off.sort_by(f64::total_cmp);
        println!(
            "| {} | {} | {} | {:.1} | {:.1} |",
            kind.as_str(),
            ran.len(),
            distribution(total),
            percentile(&off, 0.5),
            percentile(&off, 0.9)
        );
    }
    // What the request file set read and kept, and how the exact lane's
    // prepared-text cache served the calls, when the build reports them.
    let files = samples
        .iter()
        .map(|sample| rendered_numbers(&sample.waits, " files="))
        .collect::<Vec<_>>();
    let column = |rows: &[Vec<u64>], index: usize| {
        rows.iter()
            .map(|row| row.get(index).copied().unwrap_or(0) as f64)
            .collect::<Vec<_>>()
    };
    if files.iter().any(|row| row.len() >= 6) {
        let total = |index| column(&files, index).iter().sum::<f64>();
        println!(
            "\nfile set per call (p50 | p90 | max): disk reads {}; bytes read {}; peak kept bytes {}. Totals: repeat reads {:.0}, served from the set {:.0}, matching-line answers reused {:.0}",
            distribution(column(&files, 0)),
            distribution(column(&files, 1)),
            distribution(column(&files, 4)),
            total(2),
            total(3),
            total(5)
        );
    }
    let prepared = samples
        .iter()
        .map(|sample| rendered_numbers(&sample.waits, " prepared="))
        .collect::<Vec<_>>();
    if prepared.iter().any(|row| row.len() >= 4) {
        let total = |index| column(&prepared, index).iter().sum::<f64>();
        let (hits, misses) = (total(0), total(1));
        println!(
            "\nprepared exact text: {hits:.0} hits, {misses:.0} misses (hit rate {:.1}%), evicted at the entry cap {:.0}, at the byte cap {:.0}",
            100.0 * hits / (hits + misses).max(1.0),
            total(2),
            total(3)
        );
    }
}

/// The slowest tenth of `samples` by wall time: which wait kinds hold
/// their time, as shares of their summed wall time.
fn summarize_tail(title: &str, samples: &[&Sample]) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| b.waits.wall.cmp(&a.waits.wall));
    let tail = &sorted[..sorted.len().div_ceil(10)];
    if tail.is_empty() {
        return;
    }
    let wall: f64 = tail.iter().map(|sample| ms(sample.waits.wall)).sum();
    let cpu: f64 = tail
        .iter()
        .map(|sample| sample.waits.cpu.map_or(0.0, ms))
        .sum();
    println!(
        "\n{title}: slowest {} calls, wall {:.0} ms total, thread CPU {:.0} ms ({:.0}%)",
        tail.len(),
        wall,
        cpu,
        100.0 * cpu / wall.max(1e-9)
    );
    for kind in SearchWait::ALL {
        let total: f64 = tail
            .iter()
            .map(|sample| ms(sample.waits.slot(kind).total))
            .sum();
        let cpu: f64 = tail
            .iter()
            .map(|sample| ms(sample.waits.slot(kind).cpu))
            .sum();
        if total >= 0.01 * wall {
            println!(
                "- {}: {:.0} ms ({:.0}% of tail wall), off-CPU {:.0} ms",
                kind.as_str(),
                total,
                100.0 * total / wall,
                total - cpu
            );
        }
    }
}

/// Edits through AFT's `write` command until `stop`: appends a line to one
/// of `files` (cycling), every `every`, then restores each file's original
/// text at the end. Returns how many writes succeeded.
fn edit_until(ctx: &AppContext, files: &[PathBuf], every: Duration, stop: &AtomicBool) -> usize {
    let originals = files
        .iter()
        .map(|file| std::fs::read_to_string(file).unwrap())
        .collect::<Vec<_>>();
    let write = |file: &Path, content: &str, id: usize| {
        let req = request(json!({
            "id": format!("edit-{id}"),
            "command": "write",
            "session_id": "search-waits-editor",
            "file": file.display().to_string(),
            "content": content,
        }));
        aft::commands::write::handle_write(&req, ctx).success
    };
    let mut written = 0;
    let mut turn = 0;
    while !stop.load(Ordering::Relaxed) {
        let index = turn % files.len();
        let content = format!(
            "{}\n// search_waits edit {turn}\n",
            originals[index].trim_end()
        );
        written += usize::from(write(&files[index], &content, turn));
        turn += 1;
        std::thread::sleep(every);
    }
    for (index, file) in files.iter().enumerate() {
        write(file, &originals[index], turn + index);
    }
    written
}

/// Up to `count` Rust sources under 60 KB in `root`'s command sources,
/// largest first, to edit.
fn edit_targets(root: &Path, count: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("crates/aft/src/commands")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                // Mid-sized files: an agent's typical edit target, without
                // making every write reparse a 500 KB module.
                let len = entry.metadata().map_or(0, |meta| meta.len());
                if len < 60_000 {
                    found.push((len, path));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
        .into_iter()
        .take(count)
        .map(|(_, path)| path)
        .collect()
}

fn main() {
    let args = parse_args();
    let (embed_base, embed_requests) = serve_embeddings(args.embed_latency);
    eprintln!(
        "fake embedding server {embed_base}, {} ms per request",
        args.embed_latency.as_millis()
    );

    let started = Instant::now();
    // Both folders share one storage directory, as one daemon's roots do;
    // each folder is a store family of its own, so the cold one shares no
    // segments with the primary.
    let primary = bind(&args.root, &args.storage, &embed_base, "primary");
    let ready_after = wait_ready(&primary, &args.root, Duration::from_secs(1800));
    eprintln!("primary ready after {:.0} s", ready_after.as_secs_f64());
    // A first pass outside any phase so every phase starts from warm caches.
    let warm = Mutex::new(if args.warmup { args.queries.len() } else { 0 });
    run_searches(
        "warmup",
        &primary,
        &args.queries,
        1,
        false,
        &|| {
            let mut left = warm.lock().unwrap();
            let more = *left > 0;
            *left = left.saturating_sub(1);
            more
        },
        &|| String::new(),
    );

    let mut all = Vec::new();
    let mut reports = Vec::new();
    for phase in &args.phases {
        let (cpu_before, load_start) = process_cpu_and_load();
        let read_before = process_bytes_read();
        let embeds_before = embed_requests.load(Ordering::Relaxed);
        let phase_started = Instant::now();
        let samples = match phase.as_str() {
            "idle" => {
                let deadline = Instant::now() + args.phase;
                let requests = AtomicU64::new(0);
                run_searches(
                    phase,
                    &primary,
                    &args.queries,
                    args.searchers,
                    args.unique,
                    &|| {
                        Instant::now() < deadline
                            && requests.fetch_add(1, Ordering::Relaxed) < args.requests
                    },
                    &|| "none".to_string(),
                )
            }
            "cold" => {
                let cold_root = args.cold.as_ref().expect("the cold phase needs --cold");
                let cold = bind(cold_root, &args.storage, &embed_base, "cold");
                let done = AtomicBool::new(false);
                let deadline = Instant::now() + args.cold_max;
                let minimum = Instant::now() + args.phase;
                let state = Mutex::new(String::from("binding"));
                let samples = std::thread::scope(|scope| {
                    // One poller keeps the background state current, so a
                    // search reads it without a health call of its own.
                    scope.spawn(|| {
                        while !done.load(Ordering::Relaxed) {
                            let (text, semantic) = index_state(&cold, cold_root);
                            let tag = if text != "ready" {
                                "cold_build".to_string()
                            } else if semantic != "ready" {
                                "cold_fill".to_string()
                            } else {
                                "cold_ready".to_string()
                            };
                            *state.lock().unwrap() = tag;
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    });
                    let samples = run_searches(
                        phase,
                        &primary,
                        &args.queries,
                        args.searchers,
                        args.unique,
                        &|| {
                            let now = Instant::now();
                            now < deadline
                                && (now < minimum || state.lock().unwrap().as_str() != "cold_ready")
                        },
                        &|| state.lock().unwrap().clone(),
                    );
                    done.store(true, Ordering::Relaxed);
                    samples
                });
                eprintln!(
                    "cold folder state at phase end: {:?}",
                    index_state(&cold, cold_root)
                );
                samples
            }
            "edits" => {
                let targets = edit_targets(&args.root, 8);
                assert!(!targets.is_empty(), "no Rust sources to edit");
                let stop = AtomicBool::new(false);
                let deadline = Instant::now() + args.phase;
                std::thread::scope(|scope| {
                    let editor = scope.spawn(|| {
                        edit_until(&primary, &targets, Duration::from_millis(200), &stop)
                    });
                    let samples = run_searches(
                        phase,
                        &primary,
                        &args.queries,
                        args.searchers,
                        args.unique,
                        &|| Instant::now() < deadline,
                        &|| "editing".to_string(),
                    );
                    stop.store(true, Ordering::Relaxed);
                    eprintln!("edits written: {}", editor.join().unwrap());
                    samples
                })
            }
            "fresh" => {
                // Searches of a root that has just been configured: the
                // first ones meet its bind, then its own build and fill.
                let fresh_root = args.fresh.as_ref().expect("the fresh phase needs --fresh");
                let fresh = configure(fresh_root, &args.storage, &embed_base, "fresh");
                let drained = AtomicBool::new(false);
                let done = AtomicBool::new(false);
                let deadline = Instant::now() + args.cold_max;
                let minimum = Instant::now() + args.phase;
                let state = Mutex::new(String::from("fresh_bind"));
                let samples = std::thread::scope(|scope| {
                    scope.spawn(|| {
                        aft::runtime_drain::drain_deferred_configure_maintenance(&fresh);
                        drained.store(true, Ordering::Relaxed);
                    });
                    scope.spawn(|| {
                        while !done.load(Ordering::Relaxed) {
                            let tag = if !drained.load(Ordering::Relaxed) {
                                "fresh_bind".to_string()
                            } else {
                                let (text, semantic) = index_state(&fresh, fresh_root);
                                if text != "ready" {
                                    "fresh_build".to_string()
                                } else if semantic != "ready" {
                                    "fresh_fill".to_string()
                                } else {
                                    "fresh_ready".to_string()
                                }
                            };
                            *state.lock().unwrap() = tag;
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    });
                    let samples = run_searches(
                        phase,
                        &fresh,
                        &args.queries,
                        args.searchers,
                        args.unique,
                        &|| {
                            let now = Instant::now();
                            now < deadline
                                && (now < minimum
                                    || state.lock().unwrap().as_str() != "fresh_ready")
                        },
                        &|| state.lock().unwrap().clone(),
                    );
                    done.store(true, Ordering::Relaxed);
                    samples
                });
                eprintln!(
                    "fresh folder state at phase end: {:?}",
                    index_state(&fresh, fresh_root)
                );
                samples
            }
            other => panic!("unknown phase {other}"),
        };
        let (cpu_after, load_end) = process_cpu_and_load();
        reports.push(PhaseReport {
            name: phase.clone(),
            seconds: phase_started.elapsed().as_secs_f64(),
            process_cpu_seconds: cpu_after - cpu_before,
            load_start,
            load_end,
            embed_requests: embed_requests.load(Ordering::Relaxed) - embeds_before,
            bytes_read: process_bytes_read().saturating_sub(read_before),
            searches: samples.len(),
        });
        eprintln!("phase {phase}: {} searches", samples.len());
        all.extend(samples);
    }

    println!("# search_waits");
    println!(
        "\nroot `{}`, {} searchers, {} queries (unique prose: {}), embedding latency {} ms, primary ready after {:.0} s, run {:.0} s",
        args.root.display(),
        args.searchers,
        args.queries.len(),
        args.unique,
        args.embed_latency.as_millis(),
        ready_after.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
    println!(
        "\n| phase | seconds | process CPU s | load 1m start/end | embed requests | process bytes read MiB | per search KiB |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    for report in &reports {
        println!(
            "| {} | {:.1} | {:.1} | {:.1} / {:.1} | {} | {:.1} | {:.1} |",
            report.name,
            report.seconds,
            report.process_cpu_seconds,
            report.load_start,
            report.load_end,
            report.embed_requests,
            report.bytes_read as f64 / (1024.0 * 1024.0),
            report.bytes_read as f64 / 1024.0 / report.searches.max(1) as f64
        );
    }
    let mut groups: BTreeMap<(String, String), Vec<&Sample>> = BTreeMap::new();
    for sample in &all {
        groups
            .entry((sample.phase.clone(), sample.background.clone()))
            .or_default()
            .push(sample);
    }
    for ((phase, background), samples) in &groups {
        let title = format!("{phase} / {background}");
        summarize(&title, samples);
        summarize_tail(&title, samples);
    }

    if let Some(path) = &args.samples {
        let mut out = std::fs::File::create(path).unwrap();
        for sample in &all {
            let slots = SearchWait::ALL
                .iter()
                .filter(|kind| sample.waits.slot(**kind).count > 0)
                .map(|kind| {
                    let slot = sample.waits.slot(*kind);
                    (
                        kind.as_str().to_string(),
                        json!({
                            "ms": ms(slot.total),
                            "cpu_ms": ms(slot.cpu),
                            "count": slot.count,
                            "max_ms": ms(slot.max),
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            writeln!(
                out,
                "{}",
                json!({
                    "phase": sample.phase,
                    "background": sample.background,
                    "query": sample.query,
                    "success": sample.success,
                    "code": sample.code,
                    "wall_ms": ms(sample.waits.wall),
                    "cpu_ms": sample.waits.cpu.map(ms),
                    "waits": slots,
                })
            )
            .unwrap();
        }
    }
}
