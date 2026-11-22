//! Read-only daemon telemetry for the admitted Basal flow route.
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use subc_protocol::ErrorBody;

pub(super) const OPERATION: &str = "perf.sweep";
pub(super) const DISPATCH_BUDGET: Duration = Duration::from_secs(55);
const LOG_BYTES: u64 = 8 * 1024 * 1024;
const LOG_LINES: usize = 100_000;
const PROFILE_BYTES: usize = 4 * 1024 * 1024;
#[cfg(unix)]
const STDERR_BYTES: usize = 16 * 1024;
static RUNNING: AtomicBool = AtomicBool::new(false);
static IMAGE: OnceLock<Result<RunningImage, String>> = OnceLock::new();
struct RunningImage {
    file: File,
    path: PathBuf,
}

pub(super) fn initialize() {
    let _ = running_image();
}
fn running_image() -> Result<&'static RunningImage, String> {
    IMAGE
        .get_or_init(|| {
            let path = crate::pinned_executable::path().map_err(|e| e.to_string())?;
            let file = File::open(&path).map_err(|e| e.to_string())?;
            Ok(RunningImage { file, path })
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    since_ms: u64,
    #[serde(default = "default_seconds")]
    profile_seconds: u32,
}
fn default_seconds() -> u32 {
    15
}
fn arguments(envelope: &Value) -> Result<Args, ErrorBody> {
    let mut value = envelope.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("op");
    }
    let args: Args = serde_json::from_value(value).map_err(|_| {
        ErrorBody::new(
            "perf_sweep_bad_request",
            "Expected since_ms: u64 and profile_seconds: 1..30.",
        )
    })?;
    if !(1..=30).contains(&args.profile_seconds) {
        return Err(ErrorBody::new(
            "perf_sweep_bad_request",
            "profile_seconds must be in 1..30.",
        ));
    }
    Ok(args)
}

/// Held from admission through queued and running work, including discarded replies.
pub(super) struct SweepGuard<'a>(&'a AtomicBool);
impl Drop for SweepGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
fn acquire(running: &AtomicBool) -> Result<SweepGuard<'_>, ErrorBody> {
    running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| ErrorBody::new("busy", "A performance sweep is already running."))?;
    Ok(SweepGuard(running))
}
pub(super) fn admit(envelope: &Value) -> Result<SweepGuard<'static>, ErrorBody> {
    arguments(envelope)?;
    acquire(&RUNNING)
}

pub(super) fn run(envelope: &Value, limit: Instant) -> Result<Value, ErrorBody> {
    let args = arguments(envelope)?;
    let log = crate::logging::daemon_log_path();
    Ok(collect(&args, &log, LOG_BYTES, LOG_LINES, || {
        profile(args.profile_seconds, limit)
    }))
}

fn collect(
    args: &Args,
    log: &Path,
    byte_cap: u64,
    line_cap: usize,
    profile: impl FnOnce() -> Result<Value, String>,
) -> Value {
    // Save the start, not the finish, as the next window boundary so work during
    // sampling is not lost between sweeps. Second precision matches the log parser.
    let until_ms = crate::db::call_ledger::now_ms() as u64 / 1000 * 1000;
    let mut gaps = Vec::new();
    let mut gap = |name: &str, detail: String| gaps.push(json!({"name": name, "detail": detail}));
    let card =
        match running_image().and_then(|image| sha256(&image.file).map_err(|e| e.to_string())) {
            Ok(hash) => Some(hash),
            Err(error) => {
                gap("card_sha256", error);
                None
            }
        };
    let os = match os_snapshot() {
        Ok(value) => value,
        Err(error) => {
            gap("os_snapshot", error);
            json!({})
        }
    };
    let (threads, threads_total) = match profile().and_then(summarize_profile) {
        Ok(value) => value,
        Err(error) => {
            let name = error
                .split(':')
                .next()
                .filter(|name| name.starts_with("profile_"))
                .unwrap_or("profile")
                .to_owned();
            gap(&name, error);
            (Vec::new(), 0)
        }
    };
    let metrics = match File::open(log)
        .and_then(|file| parse_log_tail(file, args.since_ms, until_ms, byte_cap, line_cap))
    {
        Ok(value) => value,
        Err(error) => {
            gap("log", error.to_string());
            Value::Null
        }
    };
    json!({"pid": std::process::id(), "card_sha256": card,
        "uptime": os["uptime"], "cpu_percent": os["cpu_percent"], "rss_bytes": os["rss_bytes"],
        "load_average": load_average(), "since_ms": args.since_ms, "until_ms": until_ms,
        "profile_seconds": args.profile_seconds, "threads": threads, "threads_total": threads_total,
        "log": metrics, "gaps": gaps})
}

fn sha256(file: &File) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    #[cfg(not(unix))]
    use std::io::{Seek, SeekFrom};
    #[cfg(not(unix))]
    let mut file = {
        let mut clone = file.try_clone()?;
        clone.seek(SeekFrom::Start(0))?;
        clone
    };
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    #[cfg(unix)]
    let mut offset = 0;
    loop {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            file.read_at(&mut buffer, offset)?
        };
        #[cfg(not(unix))]
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        #[cfg(unix)]
        {
            offset += n as u64;
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(super) fn iso_seconds(ms: u64) -> String {
    let seconds = ms / 1000;
    let (year, month, day) = crate::subc_format::civil_from_days((seconds / 86400) as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[derive(Default)]
struct Samples(Vec<u64>);
impl Samples {
    fn value(&mut self) -> Value {
        self.0.sort_unstable();
        let n = self.0.len();
        if n == 0 {
            return Value::Null;
        }
        let median = if n % 2 == 0 {
            (self.0[n / 2 - 1] as f64 + self.0[n / 2] as f64) / 2.0
        } else {
            self.0[n / 2] as f64
        };
        json!({"n": n, "median": median, "p95": self.0[(0.95 * (n - 1) as f64) as usize], "max": self.0[n-1]})
    }
}
#[derive(Default)]
struct ToolSamples {
    count: usize,
    total: Samples,
    queue: Samples,
    exec: Samples,
}

fn parse_log_tail(
    mut file: File,
    since_ms: u64,
    until_ms: u64,
    byte_cap: u64,
    line_cap: usize,
) -> io::Result<Value> {
    let len = file.metadata()?.len();
    let offset = len.saturating_sub(byte_cap);
    file.seek(SeekFrom::Start(offset))?;
    let starts_at_line_boundary = if offset == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        previous[0] == b'\n'
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut input = BufReader::new(file.take(byte_cap));
    if offset > 0 && !starts_at_line_boundary {
        let mut partial = Vec::new();
        input.read_until(b'\n', &mut partial)?;
    }
    parse_log(input, since_ms, until_ms, byte_cap, line_cap)
}

fn log_timestamp(line: &str) -> Option<&str> {
    let timestamp = line.split_whitespace().next()?;
    let bytes = timestamp.as_bytes();
    if bytes.len() < 19
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    Some(timestamp)
}

fn parse_log(
    input: impl Read,
    since_ms: u64,
    until_ms: u64,
    byte_cap: u64,
    line_cap: usize,
) -> io::Result<Value> {
    // Take bounds even a single unterminated line before read_until can allocate it.
    // The extra byte distinguishes an exact fit from a truncated prefix.
    let mut input = BufReader::new(input.take(byte_cap.saturating_add(1)));
    let since = iso_seconds(since_ms);
    let until = iso_seconds(until_ms);
    let fields = Regex::new(
        r"\b(total|queue|exec|cache_key_resolve|worktree_probe|elapsed_ms|derived_ms)=(\d+)",
    )
    .unwrap();
    let tool = Regex::new(r"name=(\S+)").unwrap();
    let slot = Regex::new(r"slot acquired after (\d+)ms").unwrap();
    let session = Regex::new(r"\[ses_[^\]]+\] ").unwrap();
    let digits = Regex::new(r"\d+").unwrap();
    let mut stats: BTreeMap<&str, Samples> = [
        "slow_total",
        "slow_queue",
        "slow_exec",
        "configure_total",
        "configure_cache_key_resolve",
        "configure_worktree_probe",
        "cold_build_wait",
        "semantic_view_fill",
        "call_graph_derive",
    ]
    .into_iter()
    .map(|key| (key, Samples::default()))
    .collect();
    let mut tools: Vec<(String, ToolSamples)> = Vec::new();
    let mut tool_indices = std::collections::HashMap::new();
    let mut warns: Vec<(String, usize)> = Vec::new();
    let mut warn_indices = std::collections::HashMap::new();
    let (mut bytes, mut scanned_lines, mut lines, mut refusals, mut warn, mut error, mut panic) =
        (0u64, 0, 0, 0, 0, 0, 0);
    let mut truncated = false;
    let mut window_start = None::<String>;
    let mut buffer = Vec::new();
    while scanned_lines < line_cap {
        buffer.clear();
        let n = input.read_until(b'\n', &mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > byte_cap {
            truncated = true;
            break;
        }
        scanned_lines += 1;
        let line = String::from_utf8_lossy(&buffer);
        if window_start.is_none() {
            if let Some(timestamp) = log_timestamp(&line) {
                window_start = Some(timestamp.to_owned());
                if timestamp.get(..19).unwrap_or(timestamp) > since.as_str() {
                    truncated = true;
                }
            }
        }
        let line_timestamp = line.chars().take(19).collect::<String>();
        if line_timestamp < since || line_timestamp >= until {
            continue;
        }
        lines += 1;
        let intentional = line.contains("name=bash_watch ") || line.contains("name=bash_status ");
        let slow = line.contains("slow tool_call") && !intentional;
        let name = if slow {
            tool.captures(&line).map(|c| c[1].to_owned())
        } else {
            None
        };
        let mut tool_index = None;
        if let Some(name) = name {
            let index = *tool_indices.entry(name.clone()).or_insert_with(|| {
                tools.push((name, ToolSamples::default()));
                tools.len() - 1
            });
            tools[index].1.count += 1;
            tool_index = Some(index);
        }
        if !intentional {
            // Python re.search uses the first occurrence of each field in a line.
            let mut seen = std::collections::HashSet::new();
            for capture in fields.captures_iter(&line) {
                let field = &capture[1];
                if !seen.insert(field.to_owned()) {
                    continue;
                }
                let Ok(value) = capture[2].parse::<u64>() else {
                    continue;
                };
                if slow && matches!(field, "total" | "queue" | "exec") {
                    stats
                        .get_mut(format!("slow_{field}").as_str())
                        .unwrap()
                        .0
                        .push(value);
                    if let Some(index) = tool_index {
                        let tool = &mut tools[index].1;
                        match field {
                            "total" => &mut tool.total,
                            "queue" => &mut tool.queue,
                            _ => &mut tool.exec,
                        }
                        .0
                        .push(value);
                    }
                }
                if line.contains("configure prefix slow")
                    && matches!(field, "total" | "cache_key_resolve" | "worktree_probe")
                {
                    stats
                        .get_mut(format!("configure_{field}").as_str())
                        .unwrap()
                        .0
                        .push(value);
                }
                if line.contains("] semantic view fill root=") && field == "elapsed_ms" {
                    stats.get_mut("semantic_view_fill").unwrap().0.push(value);
                }
                if line.contains("view_publication") && field == "derived_ms" {
                    stats.get_mut("call_graph_derive").unwrap().0.push(value);
                }
            }
        }
        if let Some(capture) = slot.captures(&line) {
            if let Ok(value) = capture[1].parse() {
                stats.get_mut("cold_build_wait").unwrap().0.push(value);
            }
        }
        refusals += usize::from(line.contains("bash_start_deadline"));
        if !line.contains("bash spawned") {
            warn += usize::from(line.contains(" WARN "));
            error += usize::from(line.contains(" ERROR "));
            panic += usize::from(line.contains("panicked at"));
            if let Some((_, shape)) = line.split_once(" WARN ") {
                let shape = session.replace_all(shape, "");
                let shape: String = digits.replace_all(&shape, "N").chars().take(90).collect();
                let index = *warn_indices.entry(shape.clone()).or_insert_with(|| {
                    warns.push((shape, 0));
                    warns.len() - 1
                });
                warns[index].1 += 1;
            }
        }
    }
    if scanned_lines == line_cap && !input.fill_buf()?.is_empty() {
        truncated = true;
    }
    tools.sort_by(|a, b| b.1.count.cmp(&a.1.count));
    warns.sort_by(|a, b| b.1.cmp(&a.1));
    let warn_shapes_total = warns.len();
    let tools_total = tools.len();
    Ok(
        json!({"lines_in_window": lines, "scanned_lines": scanned_lines, "scanned_bytes": bytes.min(byte_cap), "truncated": truncated,
        "window_start": window_start,
        "stats": stats.iter_mut().map(|(key, samples)| (*key, samples.value())).collect::<BTreeMap<_, _>>(),
        "slow_by_tool": tools.into_iter().take(8).map(|(name, mut samples)| json!({"name": name, "count": samples.count,
            "total": samples.total.value(), "queue": samples.queue.value(), "exec": samples.exec.value()})).collect::<Vec<_>>(),
        "tools_total": tools_total, "bash_start_deadline": refusals, "warn": warn, "error": error, "panic": panic,
        "warn_shapes": warns.into_iter().take(8).map(|(shape, count)| json!({"shape": shape.trim(), "count": count})).collect::<Vec<_>>(),
        "warn_shapes_total": warn_shapes_total}),
    )
}

fn summarize_profile(profile: Value) -> Result<(Vec<Value>, usize), String> {
    let mut threads = profile
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| "profile_invalid: missing threads array".to_string())?
        .clone();
    if threads
        .iter()
        .any(|t| t["name"].as_str().is_none() || t["running_samples"].as_u64().is_none())
    {
        return Err("profile_invalid: thread lacks name or running_samples".into());
    }
    threads.sort_by_key(|t| std::cmp::Reverse(t["running_samples"].as_u64().unwrap()));
    let total: f64 = threads
        .iter()
        .map(|t| t["running_samples"].as_u64().unwrap() as f64)
        .sum();
    let count = threads.len();
    let image = Regex::new(r" \(in [^)]+\)").unwrap();
    let crate_hash = Regex::new(r"(aft|std|core|alloc|rusqlite)\[[0-9a-f]+\]::").unwrap();
    let noise = Regex::new(
        r"thread_start|_pthread|spawn_unchecked|rust_begin|FnOnce|lifecycle|call_once|\?\?\?",
    )
    .unwrap();
    let threads = threads.into_iter().take(8).map(|t| {
        let stacks = t["heaviest_paths"].as_array().into_iter().flatten().take(1).map(|p| {
            let frames: Vec<String> = p["frames"].as_array().into_iter().flatten().map(|f| {
                let f = f.as_str().map(str::to_owned).unwrap_or_else(|| f.get("symbol").map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_else(|| f.to_string()));
                crate_hash.replace_all(&image.replace_all(&f, ""), "").into_owned()
            }).collect();
            let mut keep: Vec<_> = frames.iter().filter(|f| !noise.is_match(f)).collect();
            if keep.is_empty() { keep = frames.iter().skip(frames.len().saturating_sub(4)).collect(); }
            else { keep = keep.into_iter().rev().take(6).collect(); keep.reverse(); }
            json!({"samples": p.get("samples").filter(|v| v.as_u64().unwrap_or(0) != 0).or_else(|| p.get("count")),
                "frames": keep.into_iter().map(|f| f.chars().take(80).collect::<String>()).collect::<Vec<_>>()})
        }).collect::<Vec<_>>();
        json!({"id": t["id"], "name": t["name"], "running_samples": t["running_samples"],
            "share_percent": 100.0 * t["running_samples"].as_u64().unwrap() as f64 / total.max(1.0), "top_stacks": stacks})
    }).collect();
    Ok((threads, count))
}

fn profile(seconds: u32, limit: Instant) -> Result<Value, String> {
    let image = running_image().map_err(|e| format!("profile_spawn: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let pinned = image
            .file
            .metadata()
            .map_err(|e| format!("profile_spawn: {e}"))?;
        let current = std::fs::metadata(&image.path).map_err(|e| format!("profile_spawn: {e}"))?;
        if (pinned.dev(), pinned.ino()) != (current.dev(), current.ino()) {
            return Err(
                "profile_executable_changed: the running executable was replaced on disk".into(),
            );
        }
    }
    let exe = &image.path;
    let deadline = (Instant::now() + Duration::from_secs(u64::from(seconds) + 5)).min(limit);
    profile_result(child_output(
        &exe,
        &[
            "profile",
            "--pid",
            &std::process::id().to_string(),
            "--seconds",
            &seconds.to_string(),
            "--json",
        ],
        deadline,
        PROFILE_BYTES,
    ))
}

fn profile_result(result: Result<Vec<u8>, String>) -> Result<Value, String> {
    let bytes = result.map_err(|error| {
        // The CLI preserves an unsymbolicated sample only after debug-artifact
        // lookup or symbolication fails. Sampling failures keep their own cause.
        if error.starts_with("exit:") && error.contains("unsymbolicated sample") {
            format!("profile_symbolication_failed: {error}")
        } else {
            format!("profile_{error}")
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|e| format!("profile_invalid: {e}"))
}

#[cfg(unix)]
fn load_average() -> Option<[f64; 3]> {
    let mut load = [0.0; 3];
    // SAFETY: load has space for the three doubles passed to getloadavg.
    (unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } == 3).then_some(load)
}
#[cfg(not(unix))]
fn load_average() -> Option<[f64; 3]> {
    None
}

fn os_snapshot() -> Result<Value, String> {
    let bytes = child_output(
        Path::new("/bin/ps"),
        &[
            "-o",
            "%cpu=,rss=,etime=",
            "-p",
            &std::process::id().to_string(),
        ],
        Instant::now() + Duration::from_secs(2),
        4096,
    )?;
    let text = String::from_utf8_lossy(&bytes);
    let mut fields = text.split_whitespace();
    let cpu = fields
        .next()
        .and_then(|f| f.parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .ok_or("ps missing CPU")?;
    let rss = fields
        .next()
        .and_then(|f| f.parse::<u64>().ok())
        .and_then(|n| n.checked_mul(1024))
        .ok_or("ps missing RSS")?;
    let uptime = fields.next().ok_or("ps missing uptime")?;
    Ok(json!({"cpu_percent": cpu, "rss_bytes": rss, "uptime": uptime}))
}

#[cfg(not(unix))]
fn child_output(_: &Path, _: &[&str], _: Instant, _: usize) -> Result<Vec<u8>, String> {
    Err("unsupported: native sampling requires Unix".into())
}

#[cfg(unix)]
struct ChildCapture {
    read: File,
    bytes: Vec<u8>,
    cap: usize,
    tail: bool,
    truncated: bool,
    eof: bool,
}

#[cfg(unix)]
impl ChildCapture {
    fn pair(cap: usize, tail: bool) -> io::Result<(Self, File)> {
        use std::os::fd::OwnedFd;
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        read.set_nonblocking(true)?;
        Ok((
            Self {
                read: File::from(OwnedFd::from(read)),
                bytes: Vec::new(),
                cap,
                tail,
                truncated: false,
                eof: false,
            },
            File::from(OwnedFd::from(write)),
        ))
    }

    /// Bound each turn so a noisy stderr cannot starve stdout or the deadline.
    fn drain(&mut self, deadline: Instant) -> Result<bool, String> {
        if self.eof {
            return Ok(false);
        }
        let mut buffer = [0u8; 8192];
        for _ in 0..4 {
            if Instant::now() >= deadline {
                return Err("timeout: child exceeded its hard deadline".into());
            }
            let limit = if self.tail {
                buffer.len()
            } else {
                self.cap
                    .saturating_sub(self.bytes.len())
                    .saturating_add(1)
                    .min(buffer.len())
            };
            match self.read.read(&mut buffer[..limit]) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(false);
                }
                Ok(n) if self.tail => {
                    let omitted = (self.bytes.len() + n).saturating_sub(self.cap);
                    self.truncated |= omitted > 0;
                    if n >= self.cap {
                        self.bytes.clear();
                        self.bytes.extend_from_slice(&buffer[n - self.cap..n]);
                    } else {
                        self.bytes.drain(..omitted);
                        self.bytes.extend_from_slice(&buffer[..n]);
                    }
                }
                Ok(n) => {
                    if self.bytes.len() + n > self.cap {
                        return Err(format!("output_limit: child exceeded {} bytes", self.cap));
                    }
                    self.bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read: {e}")),
            }
        }
        Ok(true)
    }

    fn failure(&self, reason: String) -> String {
        if self.bytes.is_empty() {
            return format!("{reason}; stderr: <empty>");
        }
        format!(
            "{reason}; stderr tail{}:\n{}",
            if self.truncated { " (truncated)" } else { "" },
            String::from_utf8_lossy(&self.bytes)
        )
    }
}

#[cfg(unix)]
fn child_output(
    exe: &Path,
    args: &[&str],
    deadline: Instant,
    cap: usize,
) -> Result<Vec<u8>, String> {
    use std::os::fd::AsRawFd;
    // An explicit native-only spawn refuses unavailable backends rather than
    // silently forking a daemon thread that may hold allocator locks.
    let (mut stdout, out) = ChildCapture::pair(cap, false).map_err(|e| format!("spawn: {e}"))?;
    let (mut stderr, err) =
        ChildCapture::pair(STDERR_BYTES, true).map_err(|e| format!("spawn: {e}"))?;
    let null = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|e| format!("spawn: {e}"))?;
    let mut command = crate::effective_path::new_command(exe).build();
    command.args(args);
    let mut child = crate::posix_spawn::try_spawn(
        &command,
        false,
        false,
        &crate::posix_spawn::PipeIo {
            gated_stdin: false,
            stdout: &out,
            stderr: &err,
            exit: null.as_raw_fd(),
            failure: null.as_raw_fd(),
        },
    )
    .ok_or("spawn: posix_spawn backend unavailable (fork refused)")?
    .map_err(|e| format!("spawn: {e}"))?;
    drop((out, err));
    let result = (|| {
        loop {
            stderr.drain(deadline)?;
            stdout.drain(deadline)?;
            if Instant::now() >= deadline {
                return Err("timeout: child exceeded its hard deadline".into());
            }
            if let Some(status) = child.try_wait().map_err(|e| format!("wait: {e}"))? {
                // The child may have written between the preceding drain and
                // waitpid. Drain available bytes, without waiting on descendants.
                while stderr.drain(deadline)? {}
                while stdout.drain(deadline)? {}
                if !status.success() {
                    return Err(format!("exit: child {status}"));
                }
                return Ok(());
            }
            let mut poll = [
                libc::pollfd {
                    fd: if stdout.eof {
                        -1
                    } else {
                        stdout.read.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr.eof {
                        -1
                    } else {
                        stderr.read.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(50) as i32;
            // SAFETY: poll points to the two initialized descriptors for this call.
            unsafe {
                libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, wait_ms);
            }
        }
    })();
    // spawn_native creates a session led by the child. Kill sampler descendants
    // too; waiting for their inherited pipes must not extend the deadline.
    if result.is_err() {
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
    }
    let _ = child.wait();
    result
        .map(|()| stdout.bytes)
        .map_err(|reason| stderr.failure(reason))
}

#[cfg(test)]
#[path = "perf_sweep_tests.rs"]
mod tests;
