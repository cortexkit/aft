use rustc_demangle::try_demangle;
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::Read;
// The atos stdin write happens only on macOS.
#[cfg(target_os = "macos")]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

use subc_client_rs::ConsumerOptions;

const DEFAULT_SECONDS: u64 = 4;
const TOP_THREADS: usize = 5;
const TOP_SYMBOLS: usize = 10;
// The macOS `sample` text parsers compile on every platform so the fixture tests
// (a real stripped ck-aft sample) run everywhere; only macOS calls them at runtime.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SAMPLE_ROOT_COLUMN: usize = 6;
#[cfg(test)]
#[inline(never)]
#[no_mangle]
pub extern "C" fn aft_profile_probe() -> usize {
    std::hint::black_box(0xA_F7usize)
}

const WAIT_MARKERS: &[&str] = &[
    "semaphore_wait_trap",
    "semaphore_timedwait_trap",
    "__psynch_cvwait",
    "kevent",
    "__semwait_signal",
    "mach_msg2_trap",
    "__select",
    "__workq_kernreturn",
    "__ulock_wait",
    "__poll",
    "__wait4",
    "__recvfrom",
    "__accept",
    "__sigsuspend",
    "__psynch_mutexwait",
];

pub fn run(args: Vec<OsString>) -> Result<(), ProfileError> {
    let args = ProfileArgs::parse(args)?;
    if args.help {
        print_usage();
        return Ok(());
    }
    if args.memory {
        let census = match fetch_memory_census() {
            Ok(census) => census,
            Err(reason) => {
                println!("AFT memory census unavailable: no daemon is connected ({reason}).");
                return Ok(());
            }
        };
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&census).map_err(|error| {
                    ProfileError::runtime(format!("could not render memory census JSON: {error}"))
                })?
            );
        } else {
            print!("{}", render_memory_census_human(&census));
        }
        return Ok(());
    }
    if cfg!(target_os = "windows") {
        return Err(ProfileError::runtime(
            "aft profile unavailable: Windows CPU sampling is not supported",
        ));
    }

    let target = resolve_target(args.pid)?;
    let target_report = inspect_target(&target)?;
    let captured = capture_sample(target.pid, args.seconds)?;
    let mut threads = captured.threads;
    if threads.is_empty() {
        return Err(ProfileError::runtime(format!(
            "{} produced no thread samples for pid {}",
            captured.sampler, target.pid
        )));
    }

    // Symbolization needs a debug artifact that may have to be fetched. If that
    // step fails, the captured sample is still the evidence the operator came
    // for: keep it on disk and name it, rather than discarding the only copy.
    let symbolicated = resolve_debug_artifact(
        &target.image,
        &target_report.version,
        &target_report.debug_id,
        args.dsym.as_deref(),
    )
    .and_then(|debug| symbolicate(&mut threads, &debug, &target.image));
    if let Err(error) = symbolicated {
        let kept = preserve_raw_sample(target.pid, &captured.raw);
        return Err(ProfileError::runtime(match kept {
            Ok(path) => format!(
                "{error}; the unsymbolicated sample was kept at {}",
                path.display()
            ),
            Err(keep_error) => {
                format!("{error}; the unsymbolicated sample could not be kept either: {keep_error}")
            }
        }));
    }

    let mut report = build_report(target_report, captured.sampler, threads);
    if args.raw {
        report.raw_sample = Some(captured.raw);
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| ProfileError::runtime(
                format!("could not render JSON: {error}")
            ))?
        );
    } else {
        print_human(&report);
    }
    Ok(())
}

#[derive(Debug)]
pub struct ProfileError {
    message: String,
    exit_code: i32,
}

impl ProfileError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProfileError {}

fn fetch_memory_census() -> Result<serde_json::Value, String> {
    let connection_file = aft::gh_shim::configured_connection_file()
        .ok_or_else(|| "no daemon connection file was discovered".to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start daemon client: {error}"))?;
    runtime.block_on(async move {
        let consumer =
            aft::fleet_status::connect_subc_consumer(&connection_file, ConsumerOptions::default())
                .await
                .map_err(|error| format!("daemon connection failed: {error}"))?;
        // subc-client-rs 0.3.0 exposes catalog_list as its only public
        // channel-0 control primitive; it does not expose a generic control
        // request method. Do not substitute call(): that opens a data route.
        let _catalog = consumer
            .catalog_list()
            .await
            .map_err(|error| format!("channel-0 control connection failed: {error}"))?;
        Err(
            "subc-client-rs has no generic channel-0 control request API for memory.census"
                .to_string(),
        )
    })
}

#[derive(Debug)]
struct ProfileArgs {
    pid: Option<u32>,
    seconds: u64,
    dsym: Option<PathBuf>,
    json: bool,
    raw: bool,
    memory: bool,
    help: bool,
}

impl ProfileArgs {
    fn parse(args: Vec<OsString>) -> Result<Self, ProfileError> {
        let mut parsed = Self {
            pid: None,
            seconds: DEFAULT_SECONDS,
            dsym: None,
            json: false,
            raw: false,
            memory: false,
            help: false,
        };
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let arg = arg
                .into_string()
                .map_err(|_| ProfileError::usage("arguments must be valid UTF-8"))?;
            match arg.as_str() {
                "--pid" => parsed.pid = Some(parse_pid(next_value(&mut iter, "--pid")?)?),
                "--seconds" => parsed.seconds = parse_seconds(next_value(&mut iter, "--seconds")?)?,
                "--dsym" => parsed.dsym = Some(PathBuf::from(next_value(&mut iter, "--dsym")?)),
                "--json" => parsed.json = true,
                "--raw" => parsed.raw = true,
                "--memory" => parsed.memory = true,
                "--help" | "-h" => parsed.help = true,
                value if value.starts_with("--pid=") => {
                    parsed.pid = Some(parse_pid(value.trim_start_matches("--pid=").to_string())?)
                }
                value if value.starts_with("--seconds=") => {
                    parsed.seconds =
                        parse_seconds(value.trim_start_matches("--seconds=").to_string())?
                }
                value if value.starts_with("--dsym=") => {
                    let path = value.trim_start_matches("--dsym=");
                    if path.is_empty() {
                        return Err(ProfileError::usage("--dsym requires a path"));
                    }
                    parsed.dsym = Some(PathBuf::from(path));
                }
                other => {
                    return Err(ProfileError::usage(format!(
                        "unknown profile argument: {other}"
                    )))
                }
            }
        }
        Ok(parsed)
    }
}

fn next_value(
    iter: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> Result<String, ProfileError> {
    iter.next()
        .ok_or_else(|| ProfileError::usage(format!("{flag} requires a value")))?
        .into_string()
        .map_err(|_| ProfileError::usage(format!("{flag} requires a valid UTF-8 value")))
}

fn parse_pid(value: String) -> Result<u32, ProfileError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            ProfileError::usage(format!("--pid must be a positive process id, got {value}"))
        })
}

fn parse_seconds(value: String) -> Result<u64, ProfileError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| {
            ProfileError::usage(format!("--seconds must be a positive integer, got {value}"))
        })
}

fn print_usage() {
    println!(
        "aft profile [--pid <pid>] [--seconds <N>] [--dsym <path>] [--json] [--raw] [--memory]"
    );
    println!("  Profiles the running AFT subc daemon when --pid is omitted.");
    println!("  --raw includes the platform sampler output; it is omitted by default.");
}

#[derive(Debug, Clone)]
struct Target {
    pid: u32,
    image: PathBuf,
}

fn resolve_target(pid: Option<u32>) -> Result<Target, ProfileError> {
    if let Some(pid) = pid {
        return Ok(Target {
            pid,
            image: process_image(pid)?,
        });
    }

    let candidates = daemon_candidates()?;
    match candidates.as_slice() {
        [] => Err(ProfileError::runtime(
            "no running AFT subc daemon found (looked for ck-aft --subc or aft --subc)",
        )),
        [target] => Ok(target.clone()),
        many => Err(ProfileError::runtime(format!(
            "multiple running AFT subc daemons found; select one with --pid: {}",
            many.iter()
                .map(|target| format!("{} ({})", target.pid, target.image.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn daemon_candidates() -> Result<Vec<Target>, ProfileError> {
    let output = command_text(Command::new("ps").args(["-axo", "pid=,comm=,args="]))?;
    let mut targets = parse_daemon_processes(&output);
    for target in &mut targets {
        if let Ok(image) = process_image(target.pid) {
            target.image = image;
        }
    }
    Ok(targets)
}

fn parse_daemon_processes(output: &str) -> Vec<Target> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let image = PathBuf::from(fields.next()?);
            let args = fields.collect::<Vec<_>>().join(" ");
            is_aft_subc_process(&args).then_some(Target { pid, image })
        })
        .collect()
}

fn is_aft_subc_process(args: &str) -> bool {
    let mut tokens = args.split_whitespace();
    let Some(command) = tokens.next() else {
        return false;
    };
    let is_aft = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "aft" || name == "ck-aft");
    is_aft && tokens.any(|token| token == "--subc" || token.starts_with("--subc="))
}

fn process_image(pid: u32) -> Result<PathBuf, ProfileError> {
    #[cfg(target_os = "linux")]
    {
        return fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| {
            ProfileError::runtime(format!(
                "could not resolve executable for pid {pid}: {error}"
            ))
        });
    }
    #[cfg(target_os = "macos")]
    {
        let output =
            command_text(Command::new("ps").args(["-p", &pid.to_string(), "-o", "comm="]))?;
        let image = output.trim();
        if image.is_empty() {
            return Err(ProfileError::runtime(format!(
                "could not resolve executable for pid {pid}: process is not running"
            )));
        }
        return Ok(PathBuf::from(image));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ProfileError::runtime(format!(
            "aft profile unavailable: executable discovery is not implemented for pid {pid}"
        )))
    }
}

#[derive(Debug, Clone, Serialize)]
struct TargetReport {
    pid: u32,
    image: String,
    sha256: String,
    version: String,
    #[serde(rename = "uuid_or_build_id")]
    debug_id: String,
}

fn inspect_target(target: &Target) -> Result<TargetReport, ProfileError> {
    let version_output = command_text(Command::new(&target.image).arg("--version"))?;
    let version = version_output
        .split_whitespace()
        .find(|part| {
            part.chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
        })
        .unwrap_or("unknown")
        .trim()
        .to_string();
    let debug_id = binary_debug_id(&target.image)?;
    Ok(TargetReport {
        pid: target.pid,
        image: target.image.display().to_string(),
        sha256: file_sha256(&target.image)?,
        version,
        debug_id,
    })
}

fn file_sha256(path: &Path) -> Result<String, ProfileError> {
    let mut file = File::open(path).map_err(|error| {
        ProfileError::runtime(format!("could not read {}: {error}", path.display()))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            ProfileError::runtime(format!("could not hash {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize())[..16].to_string())
}

fn binary_debug_id(path: &Path) -> Result<String, ProfileError> {
    #[cfg(target_os = "macos")]
    {
        let output = command_text(Command::new("dwarfdump").arg("--uuid").arg(path))?;
        return parse_mach_uuid(&output).ok_or_else(|| {
            ProfileError::runtime(format!(
                "could not read Mach-O UUID from {}",
                path.display()
            ))
        });
    }
    #[cfg(target_os = "linux")]
    {
        let path_text = path.display().to_string();
        let output = command_text(Command::new("readelf").args(["-n", &path_text]))?;
        return parse_linux_build_id(&output).ok_or_else(|| {
            ProfileError::runtime(format!(
                "could not read ELF build-id from {}",
                path.display()
            ))
        });
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ProfileError::runtime(format!(
            "aft profile unavailable: debug-id inspection is not implemented for {}",
            path.display()
        )))
    }
}

// Only the macOS arm of `binary_debug_id` reads `dwarfdump --uuid` output.
#[cfg(target_os = "macos")]
fn parse_mach_uuid(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.split("UUID:").nth(1))
        .and_then(|tail| tail.split_whitespace().next())
        .map(normalize_debug_id)
}

#[cfg(target_os = "linux")]
fn parse_linux_build_id(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build ID:"))
        .map(normalize_debug_id)
}

fn normalize_debug_id(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .flat_map(char::to_uppercase)
        .collect()
}

/// Write a captured-but-unsymbolicated sample where the operator can find it.
/// The path carries the pid and a timestamp so repeated attempts never overwrite
/// each other.
fn preserve_raw_sample(pid: u32, raw: &str) -> Result<PathBuf, ProfileError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs());
    let path = raw_sample_path(&std::env::temp_dir(), pid, stamp)?;
    fs::write(&path, raw).map_err(|error| {
        ProfileError::runtime(format!("could not write {}: {error}", path.display()))
    })?;
    Ok(path)
}

fn raw_sample_path(
    temp_dir: &Path,
    pid: u32,
    unix_seconds: Option<u64>,
) -> Result<PathBuf, ProfileError> {
    let stamp = unix_seconds
        .ok_or_else(|| ProfileError::runtime("system clock predates the Unix epoch"))?;
    Ok(temp_dir.join(format!("aft-profile-{pid}-{stamp}.unsymbolicated.txt")))
}

fn command_text(command: &mut Command) -> Result<String, ProfileError> {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| ProfileError::runtime(format!("could not run {rendered}: {error}")))?;
    if !output.status.success() {
        return Err(ProfileError::runtime(format!(
            "{rendered} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug)]
struct CapturedSample {
    sampler: String,
    raw: String,
    threads: Vec<SampleThread>,
}

fn capture_sample(pid: u32, seconds: u64) -> Result<CapturedSample, ProfileError> {
    #[cfg(target_os = "macos")]
    {
        let path =
            std::env::temp_dir().join(format!("aft-profile-{pid}-{}.txt", std::process::id()));
        let status = Command::new("sample")
            .args([
                pid.to_string(),
                seconds.to_string(),
                "-file".to_string(),
                path.display().to_string(),
            ])
            .status()
            .map_err(|error| {
                ProfileError::runtime(format!("could not start macOS sample: {error}"))
            })?;
        if !status.success() {
            return Err(ProfileError::runtime(format!(
                "macOS sample exited with {status}; no profile was produced"
            )));
        }
        let raw = fs::read_to_string(&path).map_err(|error| {
            ProfileError::runtime(format!(
                "could not read sample output {}: {error}",
                path.display()
            ))
        })?;
        let _ = fs::remove_file(path);
        return Ok(CapturedSample {
            sampler: "macos-sample".to_string(),
            threads: parse_macos_sample(&raw),
            raw,
        });
    }
    #[cfg(target_os = "linux")]
    {
        return capture_linux_sample(pid, seconds);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, seconds);
        Err(ProfileError::runtime(
            "aft profile unavailable: this platform has no supported sampler",
        ))
    }
}

#[cfg(target_os = "linux")]
fn capture_linux_sample(pid: u32, seconds: u64) -> Result<CapturedSample, ProfileError> {
    if which::which("perf").is_ok() {
        let directory =
            std::env::temp_dir().join(format!("aft-profile-{pid}-{}", std::process::id()));
        fs::create_dir_all(&directory).map_err(|error| {
            ProfileError::runtime(format!(
                "could not create perf temporary directory: {error}"
            ))
        })?;
        let perf_data = directory.join("perf.data");
        let pid_text = pid.to_string();
        let seconds_text = seconds.to_string();
        let perf_data_text = perf_data.display().to_string();
        let record = Command::new("perf")
            .args([
                "record",
                "-g",
                "-p",
                &pid_text,
                "-o",
                &perf_data_text,
                "--",
                "sleep",
                &seconds_text,
            ])
            .output();
        if let Ok(record) = record {
            if record.status.success() {
                let script = Command::new("perf")
                    .args(["script", "-i", &perf_data_text])
                    .output();
                if let Ok(script) = script {
                    if script.status.success() {
                        let raw = String::from_utf8_lossy(&script.stdout).into_owned();
                        let _ = fs::remove_dir_all(&directory);
                        return Ok(CapturedSample {
                            sampler: "linux-perf".to_string(),
                            threads: parse_perf_script(&raw),
                            raw,
                        });
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&directory);
    }
    capture_proc_stacks(pid, seconds)
}

#[cfg(target_os = "linux")]
fn capture_proc_stacks(pid: u32, seconds: u64) -> Result<CapturedSample, ProfileError> {
    let mut threads = BTreeMap::<u32, SampleThread>::new();
    for _ in 0..seconds {
        let task_dir = PathBuf::from(format!("/proc/{pid}/task"));
        let entries = fs::read_dir(&task_dir).map_err(|error| {
            ProfileError::runtime(format!("could not inspect {}: {error}", task_dir.display()))
        })?;
        for entry in entries.flatten() {
            let tid = entry.file_name().to_string_lossy().parse::<u32>().ok();
            let Some(tid) = tid else { continue };
            let path = entry.path();
            let name = fs::read_to_string(path.join("comm"))
                .unwrap_or_else(|_| format!("Thread_{tid}"))
                .trim()
                .to_string();
            let state = fs::read_to_string(path.join("status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .find(|line| line.starts_with("State:"))
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "State: unknown".to_string());
            let stack = fs::read_to_string(path.join("stack")).unwrap_or_default();
            let thread = threads.entry(tid).or_insert_with(|| SampleThread {
                id: tid.to_string(),
                name,
                declared_samples: 0,
                nodes: Vec::new(),
            });
            thread.declared_samples += 1;
            append_proc_stack(thread, &state, &stack);
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(CapturedSample {
        sampler: "linux-proc-stack fallback (perf unavailable or denied)".to_string(),
        raw: "proc task stacks collected; pass --raw only reports platform sampler text"
            .to_string(),
        threads: threads.into_values().collect(),
    })
}

#[cfg(target_os = "linux")]
fn append_proc_stack(thread: &mut SampleThread, state: &str, stack: &str) {
    let first = thread.nodes.len();
    thread.nodes.push(SampleNode {
        parent: None,
        samples: 1,
        frame: state.to_string(),
        offset: None,
    });
    let mut parent = Some(first);
    for line in stack.lines().filter(|line| !line.trim().is_empty()) {
        let index = thread.nodes.len();
        thread.nodes.push(SampleNode {
            parent,
            samples: 1,
            frame: line.trim().to_string(),
            offset: parse_hex_address(line),
        });
        parent = Some(index);
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_macos_sample(raw: &str) -> Vec<SampleThread> {
    let mut threads = Vec::new();
    let mut current: Option<SampleThread> = None;
    let mut in_call_graph = false;
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed == "Call graph:" {
            in_call_graph = true;
            continue;
        }
        if !in_call_graph {
            continue;
        }
        if trimmed.starts_with("Binary Images:") || trimmed.starts_with("Total number") {
            break;
        }
        if let Some((samples, id, name)) = parse_thread_header(line) {
            if let Some(thread) = current.take() {
                threads.push(thread);
            }
            current = Some(SampleThread {
                id,
                name,
                declared_samples: samples,
                nodes: Vec::new(),
            });
            stack.clear();
            continue;
        }
        let Some((depth, samples, frame, offset)) = parse_sample_frame(line) else {
            continue;
        };
        let Some(thread) = current.as_mut() else {
            continue;
        };
        while stack
            .last()
            .is_some_and(|(parent_depth, _)| *parent_depth >= depth)
        {
            stack.pop();
        }
        let parent = stack.last().map(|(_, index)| *index);
        let index = thread.nodes.len();
        thread.nodes.push(SampleNode {
            parent,
            samples,
            frame,
            offset,
        });
        stack.push((depth, index));
    }
    if let Some(thread) = current {
        threads.push(thread);
    }
    threads
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_thread_header(line: &str) -> Option<(u64, String, String)> {
    let mut fields = line.split_whitespace();
    let samples = fields.next()?.parse::<u64>().ok()?;
    let raw_id = fields.next()?;
    let id = raw_id.strip_prefix("Thread_")?.trim_end_matches(':');
    Some((
        samples,
        id.to_string(),
        fields.collect::<Vec<_>>().join(" "),
    ))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_sample_frame(line: &str) -> Option<(usize, u64, String, Option<String>)> {
    let (sample_column, samples) = first_sample_column(line)?;
    // `sample` draws each tree level two columns apart. Some trees replace
    // indentation with `!`, `:`, and `|`; others omit those markers entirely.
    // The count column is stable in both forms, unlike marker presence.
    let depth = sample_column.saturating_sub(SAMPLE_ROOT_COLUMN) / 2;
    let frame = line[sample_column..]
        .split_once(char::is_whitespace)?
        .1
        .trim()
        .to_string();
    (!frame.is_empty()).then(|| (depth, samples, frame.clone(), parse_sample_offset(&frame)))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn first_sample_column(line: &str) -> Option<(usize, u64)> {
    line.char_indices().find_map(|(start, character)| {
        (character.is_ascii_digit()
            && (start == 0 || line.as_bytes()[start - 1].is_ascii_whitespace()))
        .then(|| line[start..].split_whitespace().next())
        .flatten()
        .filter(|token| token.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|token| token.parse::<u64>().ok().map(|samples| (start, samples)))
    })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_sample_offset(frame: &str) -> Option<String> {
    if let Some(tail) = frame.rsplit_once("load address ").map(|(_, tail)| tail) {
        let offset = tail.rsplit_once(" + ")?.1.split_whitespace().next()?;
        return u64::from_str_radix(offset.strip_prefix("0x")?, 16)
            .ok()
            .map(|offset| format!("0x{offset:x}"));
    }

    // A decimal `) + N` offset comes from a frame that `sample` has already
    // symbolicated. Leave it untouched; only stripped `???` frames need atos.
    let tail = frame.rsplit_once(") +")?.1.trim_start();
    tail.split_whitespace().next()?.parse::<u64>().ok()?;
    None
}

#[cfg(target_os = "linux")]
fn parse_hex_address(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|part| part.starts_with("0x"))
        .map(|part| {
            part.trim_matches(|character: char| !character.is_ascii_hexdigit() && character != 'x')
        })
        .map(ToOwned::to_owned)
}

#[cfg(target_os = "linux")]
fn parse_perf_script(raw: &str) -> Vec<SampleThread> {
    let mut threads = BTreeMap::<String, SampleThread>::new();
    let mut current: Option<String> = None;
    let mut current_parent: Option<usize> = None;
    for line in raw.lines() {
        if !line.starts_with(char::is_whitespace) && line.contains('/') && line.contains(':') {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let Some(pid_tid) = fields.get(1) else {
                continue;
            };
            let Some((_, tid)) = pid_tid.split_once('/') else {
                continue;
            };
            let key = tid.to_string();
            let name = fields.first().copied().unwrap_or("aft").to_string();
            let thread = threads.entry(key.clone()).or_insert_with(|| SampleThread {
                id: key.clone(),
                name,
                declared_samples: 0,
                nodes: Vec::new(),
            });
            thread.declared_samples += 1;
            current = Some(key);
            current_parent = None;
            continue;
        }
        let Some(key) = current.as_ref() else {
            continue;
        };
        if !line.starts_with(char::is_whitespace) {
            current = None;
            current_parent = None;
            continue;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let thread = threads.get_mut(key).expect("perf sample thread exists");
        let index = thread.nodes.len();
        thread.nodes.push(SampleNode {
            parent: current_parent,
            samples: 1,
            frame: text.to_string(),
            offset: parse_hex_address(text),
        });
        current_parent = Some(index);
    }
    threads.into_values().collect()
}

#[derive(Debug, Clone)]
struct SampleThread {
    id: String,
    name: String,
    declared_samples: u64,
    nodes: Vec<SampleNode>,
}

#[derive(Debug, Clone)]
struct SampleNode {
    parent: Option<usize>,
    samples: u64,
    frame: String,
    offset: Option<String>,
}

#[derive(Debug)]
struct DebugArtifact {
    dwarf: PathBuf,
}

fn resolve_debug_artifact(
    image: &Path,
    version: &str,
    expected_id: &str,
    supplied: Option<&Path>,
) -> Result<DebugArtifact, ProfileError> {
    if let Some(path) = supplied {
        return inspect_debug_candidate(path, image, expected_id, true)?.ok_or_else(|| {
            ProfileError::runtime(format!("debug UUID mismatch for {}", path.display()))
        });
    }

    let sibling = if cfg!(target_os = "macos") {
        PathBuf::from(format!("{}.dSYM", image.display()))
    } else {
        PathBuf::from(format!("{}.debug", image.display()))
    };
    if let Some(artifact) = inspect_debug_candidate(&sibling, image, expected_id, false)? {
        return Ok(artifact);
    }

    let cache = debug_cache_dir(expected_id)?;
    if let Some(artifact) = inspect_debug_candidate(&cache, image, expected_id, false)? {
        return Ok(artifact);
    }

    download_release_debug(version, expected_id, &cache)?;
    inspect_debug_candidate(&cache, image, expected_id, true)?.ok_or_else(|| {
        ProfileError::runtime(format!(
            "downloaded debug artifact in {} did not contain UUID/build-id {}",
            cache.display(),
            expected_id
        ))
    })
}

fn inspect_debug_candidate(
    candidate: &Path,
    image: &Path,
    expected_id: &str,
    explicit: bool,
) -> Result<Option<DebugArtifact>, ProfileError> {
    if !candidate.exists() {
        return Ok(None);
    }
    let Some(dwarf) = debug_file_in(candidate, image) else {
        if explicit {
            return Err(ProfileError::runtime(format!(
                "could not find a DWARF/debug file in {}",
                candidate.display()
            )));
        }
        return Ok(None);
    };
    let actual_id = binary_debug_id(&dwarf)?;
    validate_debug_artifact_uuid(expected_id, &actual_id, candidate)?;
    Ok(Some(DebugArtifact { dwarf }))
}

fn validate_debug_artifact_uuid(
    expected_id: &str,
    actual_id: &str,
    path: &Path,
) -> Result<(), ProfileError> {
    let expected_id = normalize_debug_id(expected_id);
    let actual_id = normalize_debug_id(actual_id);
    if expected_id == actual_id {
        return Ok(());
    }
    Err(ProfileError::runtime(format!(
        "debug UUID mismatch for {}: running image {} vs debug artifact {}",
        path.display(),
        expected_id,
        actual_id
    )))
}

fn debug_file_in(candidate: &Path, image: &Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return is_debug_file(candidate).then(|| candidate.to_path_buf());
    }
    if let Some(dwarf) = dwarf_file_in_bundle(candidate, image) {
        return Some(dwarf);
    }

    let entries = fs::read_dir(candidate).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && path
                .extension()
                .is_some_and(|extension| extension == "dSYM")
        {
            if let Some(dwarf) = dwarf_file_in_bundle(&path, image) {
                return Some(dwarf);
            }
        }
        if path.is_dir()
            && path
                .extension()
                .is_some_and(|extension| extension == "debug")
        {
            if let Some(debug) = debug_file_in_linux_layout(&path) {
                return Some(debug);
            }
        }
        if path.is_file() && is_debug_file(&path) {
            return Some(path);
        }
    }
    None
}

fn dwarf_file_in_bundle(bundle: &Path, image: &Path) -> Option<PathBuf> {
    let dwarf_dir = bundle.join("Contents").join("Resources").join("DWARF");
    if !dwarf_dir.is_dir() {
        return None;
    }
    let preferred = image.file_name().map(|name| dwarf_dir.join(name));
    if let Some(preferred) = preferred.filter(|path| path.is_file()) {
        return Some(preferred);
    }
    fs::read_dir(dwarf_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_file())
}

fn debug_file_in_linux_layout(layout: &Path) -> Option<PathBuf> {
    fs::read_dir(layout)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_file() && is_debug_file(path))
}

fn is_debug_file(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "DWARF")
        || path
            .extension()
            .is_some_and(|extension| extension == "debug" || extension == "dwp")
}

fn debug_cache_dir(debug_id: &str) -> Result<PathBuf, ProfileError> {
    Ok(debug_cache_dir_from(
        &aft::bash_background::storage_dir(None),
        debug_id,
    ))
}

fn debug_cache_dir_from(storage_root: &Path, debug_id: &str) -> PathBuf {
    // Profiling artifacts belong to the same storage universe as the binary
    // being profiled. Reusing the module root keeps explicit, compatibility,
    // and platform data-home rungs from being copied into this CLI.
    storage_root.join("dsym").join(normalize_debug_id(debug_id))
}

fn download_release_debug(
    version: &str,
    expected_id: &str,
    cache: &Path,
) -> Result<(), ProfileError> {
    let asset_name = release_debug_asset_name();
    let release_url = format!(
        "https://api.github.com/repos/cortexkit/aft/releases/tags/v{}",
        version.trim_start_matches('v')
    );
    let client = reqwest::blocking::Client::builder()
        .user_agent("aft-profile")
        .build()
        .map_err(|error| {
            ProfileError::runtime(format!("could not create GitHub client: {error}"))
        })?;
    let release: serde_json::Value = client
        .get(&release_url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| {
            ProfileError::runtime(format!(
                "could not download release metadata for v{version}: {error}"
            ))
        })?
        .json()
        .map_err(|error| {
            ProfileError::runtime(format!(
                "could not parse release metadata for v{version}: {error}"
            ))
        })?;
    let asset_url = release
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                (asset.get("name")?.as_str()? == asset_name)
                    .then(|| {
                        asset
                            .get("browser_download_url")?
                            .as_str()
                            .map(ToOwned::to_owned)
                    })
                    .flatten()
            })
        })
        .ok_or_else(|| {
            ProfileError::runtime(format!(
                "release v{version} has no {asset_name} debug asset for {}",
                std::env::consts::OS
            ))
        })?;
    // The debug asset is an immutable release artifact of several MB; a body
    // read cut mid-stream by a network blip is worth a couple of retries
    // before the operator loses the profile over it.
    let mut attempt = 0u32;
    let bytes = loop {
        attempt += 1;
        let result = client
            .get(&asset_url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| format!("could not download {asset_name}: {error}"))
            .and_then(|response| {
                response
                    .bytes()
                    .map_err(|error| format!("could not read {asset_name}: {error}"))
            });
        match result {
            Ok(bytes) => break bytes,
            Err(message) if attempt < 3 => {
                std::thread::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)));
                eprintln!("{message}; retrying ({attempt}/3)");
            }
            Err(message) => return Err(ProfileError::runtime(message)),
        }
    };
    fs::create_dir_all(cache).map_err(|error| {
        ProfileError::runtime(format!(
            "could not create dSYM cache {}: {error}",
            cache.display()
        ))
    })?;
    let archive = cache.join(&asset_name);
    fs::write(&archive, bytes)
        .map_err(|error| ProfileError::runtime(format!("could not cache {asset_name}: {error}")))?;
    let archive_path = archive.display().to_string();
    let cache_path = cache.display().to_string();
    let mut extract = if cfg!(target_os = "macos") {
        let mut command = Command::new("ditto");
        command.args(["-x", "-k", &archive_path, &cache_path]);
        command
    } else {
        let mut command = Command::new("unzip");
        command.args(["-q", &archive_path, "-d", &cache_path]);
        command
    };
    command_text(&mut extract).map_err(|error| {
        ProfileError::runtime(format!(
            "could not extract {asset_name} for UUID/build-id {expected_id}: {error}"
        ))
    })?;
    let _ = fs::remove_file(archive);
    Ok(())
}

fn release_debug_asset_name() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    if cfg!(target_os = "macos") {
        format!("aft-darwin-{arch}-dsym.zip")
    } else {
        format!("aft-linux-{arch}-dwp.zip")
    }
}

fn symbolicate(
    threads: &mut [SampleThread],
    debug: &DebugArtifact,
    image: &Path,
) -> Result<(), ProfileError> {
    #[cfg(target_os = "macos")]
    {
        let offsets = image_offsets(threads, image);
        let symbols = symbolicate_macos(&debug.dwarf, &offsets)?;
        for thread in threads {
            for node in &mut thread.nodes {
                if let Some(symbol) = node
                    .offset
                    .as_ref()
                    .filter(|_| frame_belongs_to_image(&node.frame, image))
                    .and_then(|offset| symbols.get(offset))
                {
                    node.frame = demangle_rust_symbols(symbol);
                } else {
                    node.frame = demangle_rust_symbols(&node.frame);
                }
            }
        }
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let addresses = image_offsets(threads, image);
        let symbols = symbolicate_linux(&debug.dwarf, &addresses)?;
        for thread in threads {
            for node in &mut thread.nodes {
                if let Some(symbol) = node
                    .offset
                    .as_ref()
                    .filter(|_| frame_belongs_to_image(&node.frame, image))
                    .and_then(|address| symbols.get(address))
                {
                    if !symbol.starts_with("??") {
                        node.frame = demangle_rust_symbols(symbol);
                    }
                }
                node.frame = demangle_rust_symbols(&node.frame);
            }
        }
        return Ok(());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (threads, debug, image);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn symbolicate_macos(
    dwarf: &Path,
    offsets: &[String],
) -> Result<HashMap<String, String>, ProfileError> {
    let unique = unique_values(offsets);
    if unique.is_empty() {
        return Ok(HashMap::new());
    }
    let mut child = Command::new("atos")
        .args([
            "-o",
            &dwarf.display().to_string(),
            "-arch",
            "arm64",
            "-offset",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| ProfileError::runtime(format!("could not start atos: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| ProfileError::runtime("atos did not accept offset input"))?
        .write_all(unique.join("\n").as_bytes())
        .map_err(|error| ProfileError::runtime(format!("could not write atos offsets: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| ProfileError::runtime(format!("could not wait for atos: {error}")))?;
    if !output.status.success() {
        return Err(ProfileError::runtime(format!(
            "atos failed for {}: {}",
            dwarf.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(unique
        .into_iter()
        .zip(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(ToOwned::to_owned),
        )
        .collect())
}

#[cfg(target_os = "linux")]
fn symbolicate_linux(
    debug: &Path,
    addresses: &[String],
) -> Result<HashMap<String, String>, ProfileError> {
    let unique = unique_values(addresses);
    if unique.is_empty() {
        return Ok(HashMap::new());
    }
    let debug_path = debug.display().to_string();
    let addresses = unique.join(" ");
    let output = command_text(Command::new("addr2line").args([
        "-C",
        "-f",
        "-p",
        "-e",
        &debug_path,
        &addresses,
    ]))?;
    Ok(unique
        .into_iter()
        .zip(output.lines().map(ToOwned::to_owned))
        .collect())
}

fn image_offsets(threads: &[SampleThread], image: &Path) -> Vec<String> {
    threads
        .iter()
        .flat_map(|thread| {
            thread.nodes.iter().filter_map(|node| {
                frame_belongs_to_image(&node.frame, image)
                    .then(|| node.offset.clone())
                    .flatten()
            })
        })
        .collect()
}

fn frame_belongs_to_image(frame: &str, image: &Path) -> bool {
    let image_name = image
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    frame.contains(&format!("(in {image_name})"))
        || frame.contains(&format!("(in {})", image.display()))
}

fn unique_values(values: &[String]) -> Vec<String> {
    let mut unique = values.to_vec();
    unique.sort();
    unique.dedup();
    unique
}

fn demangle_rust_symbols(text: &str) -> String {
    text.split_whitespace()
        .map(|part| {
            let trimmed = part.trim_matches(|character: char| "(),[]".contains(character));
            let Some(demangled) = try_demangle(trimmed).ok() else {
                return part.to_string();
            };
            part.replacen(trimmed, &demangled.to_string(), 1)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Serialize)]
struct ProfileReport {
    target: TargetReport,
    sampler: String,
    threads: Vec<ThreadReport>,
    top_symbols: Vec<SymbolSamples>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_sample: Option<String>,
}

#[derive(Debug, Serialize)]
struct ThreadReport {
    id: String,
    name: String,
    total_samples: u64,
    leaf_samples: u64,
    parse_inconsistent: bool,
    running_samples: u64,
    waiting_samples: u64,
    verdict: String,
    heaviest_paths: Vec<HotPath>,
}

#[derive(Debug, Serialize)]
struct HotPath {
    samples: u64,
    frames: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SymbolSamples {
    symbol: String,
    inclusive_samples: u64,
}

fn build_report(
    target: TargetReport,
    sampler: String,
    threads: Vec<SampleThread>,
) -> ProfileReport {
    let mut reports = threads.iter().map(thread_report).collect::<Vec<_>>();
    reports.sort_by(|left, right| {
        left.parse_inconsistent
            .cmp(&right.parse_inconsistent)
            .then_with(|| right.running_samples.cmp(&left.running_samples))
    });
    let mut inclusive = HashMap::<String, u64>::new();
    for thread in &threads {
        if thread_report(thread).parse_inconsistent {
            continue;
        }
        for (samples, path) in running_leaf_paths(thread) {
            for index in path {
                *inclusive
                    .entry(thread.nodes[index].frame.clone())
                    .or_default() += samples;
            }
        }
    }
    let mut top_symbols = inclusive
        .into_iter()
        .map(|(symbol, inclusive_samples)| SymbolSamples {
            symbol,
            inclusive_samples,
        })
        .collect::<Vec<_>>();
    top_symbols.sort_by(|left, right| {
        right
            .inclusive_samples
            .cmp(&left.inclusive_samples)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });
    top_symbols.truncate(TOP_SYMBOLS);
    ProfileReport {
        target,
        sampler,
        threads: reports,
        top_symbols,
        raw_sample: None,
    }
}

fn thread_report(thread: &SampleThread) -> ThreadReport {
    let leaves = leaf_paths(thread);
    let leaf_samples = leaves.iter().map(|(samples, _)| *samples).sum::<u64>();
    let parse_inconsistent = leaf_samples != thread.declared_samples;
    let total_samples = thread.declared_samples;
    let running_paths = leaves
        .into_iter()
        .filter(|(_, path)| {
            path.last()
                .is_some_and(|index| !is_wait_frame(&thread.nodes[*index].frame))
        })
        .collect::<Vec<_>>();
    let running_samples: u64 = running_paths.iter().map(|(samples, _)| *samples).sum();
    let threshold = running_samples.div_ceil(4);
    let mut heaviest_paths = running_paths
        .into_iter()
        .filter(|(samples, _)| *samples >= threshold && *samples > 0)
        .map(|(samples, path)| HotPath {
            samples,
            frames: path
                .into_iter()
                .map(|index| thread.nodes[index].frame.clone())
                .collect(),
        })
        .collect::<Vec<_>>();
    heaviest_paths.sort_by(|left, right| right.samples.cmp(&left.samples));
    ThreadReport {
        id: thread.id.clone(),
        name: if thread.name.is_empty() {
            format!("Thread_{}", thread.id)
        } else {
            thread.name.clone()
        },
        total_samples,
        leaf_samples,
        parse_inconsistent,
        running_samples,
        waiting_samples: total_samples.saturating_sub(running_samples),
        verdict: subsystem_for_paths(&heaviest_paths),
        heaviest_paths,
    }
}

fn leaf_paths(thread: &SampleThread) -> Vec<(u64, Vec<usize>)> {
    let mut parents = vec![false; thread.nodes.len()];
    for node in &thread.nodes {
        if let Some(parent) = node.parent {
            if parent < parents.len() {
                parents[parent] = true;
            }
        }
    }
    thread
        .nodes
        .iter()
        .enumerate()
        .filter(|(index, node)| !parents[*index] && node.samples > 0)
        .map(|(index, node)| (node.samples, node_path(thread, index)))
        .collect()
}

fn running_leaf_paths(thread: &SampleThread) -> Vec<(u64, Vec<usize>)> {
    leaf_paths(thread)
        .into_iter()
        .filter(|(_, path)| {
            path.last()
                .is_some_and(|index| !is_wait_frame(&thread.nodes[*index].frame))
        })
        .collect()
}

fn node_path(thread: &SampleThread, mut index: usize) -> Vec<usize> {
    let mut path = vec![index];
    while let Some(parent) = thread.nodes[index].parent {
        path.push(parent);
        index = parent;
    }
    path.reverse();
    path
}

fn is_wait_frame(frame: &str) -> bool {
    let lower = frame.to_ascii_lowercase();
    WAIT_MARKERS.iter().any(|marker| lower.contains(marker))
        || lower.starts_with("read ")
        || lower.contains(" read ")
        || lower.contains("`read")
}

fn subsystem_for_paths(paths: &[HotPath]) -> String {
    let frames = paths
        .iter()
        .flat_map(|path| path.frames.iter())
        .map(|frame| frame.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if frames.contains("callgraph_store") || frames.contains("callgraph") {
        "callgraph_store".to_string()
    } else if frames.contains("search_index") || frames.contains("search index") {
        "search_index".to_string()
    } else if frames.contains("semantic") || frames.contains("embedding") || frames.contains("onnx")
    {
        "semantic".to_string()
    } else if frames.contains("lsp") {
        "lsp".to_string()
    } else if frames.contains("subc") {
        "subc loop".to_string()
    } else if frames.contains("inspect") {
        "inspect".to_string()
    } else if frames.contains("bash_background") || frames.contains("background_task") {
        "bash_background".to_string()
    } else {
        "other".to_string()
    }
}

fn human_hot_path_frames(path: &HotPath) -> Vec<String> {
    let first_useful = path
        .frames
        .iter()
        .position(|frame| !is_runtime_prologue(frame))
        .unwrap_or_else(|| path.frames.len().saturating_sub(1));
    let frames = &path.frames[first_useful..];
    let omitted = frames.len().saturating_sub(12);
    let mut rendered = Vec::with_capacity(12 + usize::from(omitted > 0));
    if omitted > 0 {
        rendered.push(format!("… ({omitted} more)"));
    }
    rendered.extend(frames[omitted..].iter().cloned());
    rendered
}

fn is_runtime_prologue(frame: &str) -> bool {
    frame.starts_with("start ") && frame.contains("(in dyld)")
        || frame.starts_with("main ") && frame.contains("(in ")
        || frame.contains("::main (in ")
        || frame.contains("thread_start")
        || frame.contains("_pthread_start")
        || frame.contains("__rust_begin_short_backtrace")
        || frame.contains("{closure")
        || frame.contains("{{closure}}")
        || frame.contains("FnOnce") && frame.contains("call_once")
}

/// Render the daemon census without truncating roots. Keeping this separate
/// from sampling makes the column contract testable without a live daemon.
pub fn render_memory_census_human(value: &serde_json::Value) -> String {
    let mut output = String::new();
    let process = &value["process"];
    writeln!(&mut output, "AFT memory census").unwrap();
    writeln!(
        &mut output,
        "phys footprint: {} MB",
        mb(process["phys_footprint_bytes"].as_u64().unwrap_or(0))
    )
    .unwrap();
    writeln!(
        &mut output,
        "rss: {} MB",
        mb(process["rss_bytes"].as_u64().unwrap_or(0))
    )
    .unwrap();
    writeln!(
        &mut output,
        "allocator slack (reclaimable by relief): {} MB",
        mb(process["allocator_slack_bytes"].as_u64().unwrap_or(0))
    )
    .unwrap();
    writeln!(
        &mut output,
        "sqlite: {} MB",
        mb(process["sqlite_bytes"].as_u64().unwrap_or(0))
    )
    .unwrap();
    writeln!(
        &mut output,
        "total attributed: {} MB; unattributed: {} MB",
        mb(process["total_attributed_bytes"].as_u64().unwrap_or(0)),
        mb_signed(process["unattributed_bytes"].as_i64().unwrap_or(0))
    )
    .unwrap();
    writeln!(&mut output, "\nroot (short: basename, worktree pool ids kept) | bound | idle | search semantic symbols callgraph inspect | attributed | evictable | evicts in | lsp").unwrap();
    let mut roots = value["roots"]
        .as_object()
        .map(|roots| roots.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    roots.sort_by(|(_, left), (_, right)| {
        right["attributed_bytes"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&left["attributed_bytes"].as_u64().unwrap_or(0))
    });
    for (root, row) in roots {
        let planes = &row["planes"];
        let short = std::path::Path::new(root)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(root);
        let horizon = row["evictable_in_ms"]
            .as_u64()
            .map(|ms| format!("{}ms", ms))
            .unwrap_or_else(|| "—".to_string());
        writeln!(
            &mut output,
            "{} ({}) | {} | {}ms | {} {} {} {} {} | {} | {} | {} | {}",
            root,
            short,
            row["bound_routes"],
            row["last_request_age_ms"],
            mb(planes["search"].as_u64().unwrap_or(0)),
            mb(planes["semantic"].as_u64().unwrap_or(0)),
            mb(planes["symbols"].as_u64().unwrap_or(0)),
            mb(planes["callgraph"].as_u64().unwrap_or(0)),
            mb(planes["inspect"].as_u64().unwrap_or(0)),
            mb(row["attributed_bytes"].as_u64().unwrap_or(0)),
            mb(row["evictable_bytes"].as_u64().unwrap_or(0)),
            horizon,
            row["lsp_children"]["count"].as_u64().unwrap_or(0)
        )
        .unwrap();
    }
    output
}

fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}
fn mb_signed(bytes: i64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn print_human(report: &ProfileReport) {
    println!("AFT CPU profile ({})", report.sampler);
    println!("pid: {}", report.target.pid);
    println!("image: {}", report.target.image);
    println!("sha256: {}", report.target.sha256);
    println!("version: {}", report.target.version);
    println!("uuid/build-id: {}", report.target.debug_id);
    println!("\nThread census (running / total):");
    let mut threads = report.threads.iter().collect::<Vec<_>>();
    threads.sort_by(|left, right| right.running_samples.cmp(&left.running_samples));
    for thread in threads {
        if thread.parse_inconsistent {
            println!(
                "  {} {}: (parse inconsistent: leaves={} declared={}) — {}",
                thread.id, thread.name, thread.leaf_samples, thread.total_samples, thread.verdict
            );
        } else {
            println!(
                "  {} {}: {} / {} running ({} waiting) — {}",
                thread.id,
                thread.name,
                thread.running_samples,
                thread.total_samples,
                thread.waiting_samples,
                thread.verdict
            );
        }
    }
    println!("\nHot running threads:");
    for thread in report
        .threads
        .iter()
        .filter(|thread| !thread.parse_inconsistent && thread.running_samples > 0)
        .take(TOP_THREADS)
    {
        for path in &thread.heaviest_paths {
            println!(
                "  {} ({}): {}",
                thread.id,
                path.samples,
                human_hot_path_frames(path).join(" > ")
            );
        }
    }
    println!("\nTop inclusive running symbols:");
    for symbol in &report.top_symbols {
        println!("  {:>5} {}", symbol.inclusive_samples, symbol.symbol);
    }
    if let Some(raw) = &report.raw_sample {
        println!("\nRaw sampler output:\n{raw}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_cache_is_derived_from_the_injected_shared_storage_root() {
        assert_eq!(
            debug_cache_dir_from(Path::new("/storage/cortexkit/aft"), "ab-cd"),
            PathBuf::from("/storage/cortexkit/aft/dsym/ABCD")
        );
    }

    #[test]
    fn unavailable_profile_timestamp_is_not_substituted_into_an_outbound_path() {
        let error = raw_sample_path(Path::new("/tmp"), 42, None).expect_err("timestamp absent");
        assert!(error.to_string().contains("system clock predates"));
        assert_eq!(
            raw_sample_path(Path::new("/tmp"), 42, Some(17)).unwrap(),
            PathBuf::from("/tmp/aft-profile-42-17.unsymbolicated.txt")
        );
    }

    #[test]
    fn unsymbolicated_sample_is_kept_on_disk_and_named() {
        let raw = "Sampling process 1 for 1 seconds\nThread_1\n  1 main\n";
        let path = preserve_raw_sample(424242, raw).expect("keep sample");
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("aft-profile-424242-"));
        assert_eq!(fs::read_to_string(&path).expect("read kept sample"), raw);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn memory_renderer_sorts_and_keeps_all_roots() {
        let mut roots = serde_json::Map::new();
        for index in 0..30 {
            roots.insert(
                format!("/worktree/pool-{index:02}"),
                serde_json::json!({
                    "bound_routes": 0,
                    "last_request_age_ms": 0,
                    "evictable_in_ms": 0,
                    "planes": {"search": 0, "semantic": 0, "symbols": 0, "callgraph": 0, "inspect": 0},
                    "attributed_bytes": 30 - index,
                    "evictable_bytes": 0,
                    "lsp_children": {"count": 0}
                }),
            );
        }
        let rendered = render_memory_census_human(&serde_json::json!({
            "roots": roots,
            "process": {}
        }));
        let lines = rendered.lines().collect::<Vec<_>>();
        let root_lines = lines
            .iter()
            .filter(|line| line.contains("/worktree/pool-"))
            .collect::<Vec<_>>();
        assert_eq!(root_lines.len(), 30);
        assert!(root_lines[0].contains("pool-00"));
        assert!(root_lines[29].contains("pool-29"));
    }

    #[test]
    fn parses_real_stripped_macos_sample_into_census_and_symbolication_queue() {
        let raw = include_str!("../../tests/fixtures/ck-aft-sample.txt");
        let threads = parse_macos_sample(raw);
        assert_eq!(threads.len(), 3);

        let main_thread = threads
            .iter()
            .find(|thread| thread.id == "13406187")
            .unwrap();
        let stripped = main_thread
            .nodes
            .iter()
            .find(|node| node.frame.contains("load address 0x1041c0000 + 0x61c44"))
            .unwrap();
        assert_eq!(stripped.offset.as_deref(), Some("0x61c44"));
        let stripped_index = main_thread
            .nodes
            .iter()
            .position(|node| node.frame == stripped.frame)
            .unwrap();
        let kevent_index = main_thread
            .nodes
            .iter()
            .position(|node| node.frame.starts_with("kevent  "))
            .unwrap();
        assert_eq!(node_path(main_thread, stripped_index).len(), 2);
        assert_eq!(node_path(main_thread, kevent_index).len(), 9);
        assert!(image_offsets(&threads, Path::new("/tmp/ck-aft")).contains(&"0x61c44".to_string()));

        let target = TargetReport {
            pid: 42,
            image: "/tmp/ck-aft".to_string(),
            sha256: "deadbeef".to_string(),
            version: "0.55.1".to_string(),
            debug_id: "ABC".to_string(),
        };
        assert!(threads.iter().all(|thread| {
            leaf_paths(thread)
                .iter()
                .map(|(samples, _)| *samples)
                .sum::<u64>()
                == thread.declared_samples
        }));
        let report = build_report(target, "macos-sample".to_string(), threads);
        assert!(report
            .threads
            .iter()
            .all(|thread| !thread.parse_inconsistent));
        let log_writer = report
            .threads
            .iter()
            .find(|thread| thread.id == "13406262")
            .unwrap();
        let parked = report
            .threads
            .iter()
            .find(|thread| thread.id == "13406263")
            .unwrap();
        assert_eq!(log_writer.running_samples, 0);
        assert_eq!(parked.running_samples, 0);
    }

    #[test]
    fn parses_unmarked_sample_tree_without_false_leaves() {
        let raw = include_str!("../../tests/fixtures/ck-aft-unmarked-thread.txt");
        let threads = parse_macos_sample(raw);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.declared_samples, 1163);
        assert_eq!(
            leaf_paths(thread)
                .iter()
                .map(|(samples, _)| *samples)
                .sum::<u64>(),
            1163
        );
        assert!(!thread_report(thread).parse_inconsistent);
    }

    #[test]
    fn inconsistent_leaf_totals_are_marked_and_not_ranked_as_hot() {
        let inconsistent = SampleThread {
            id: "inconsistent".to_string(),
            name: "broken tree".to_string(),
            declared_samples: 10,
            nodes: vec![
                SampleNode {
                    parent: None,
                    samples: 10,
                    frame: "work-a".to_string(),
                    offset: None,
                },
                SampleNode {
                    parent: None,
                    samples: 10,
                    frame: "work-b".to_string(),
                    offset: None,
                },
            ],
        };
        let report = thread_report(&inconsistent);
        assert!(report.parse_inconsistent);
        assert_eq!(report.leaf_samples, 20);
        assert_eq!(report.total_samples, 10);

        let target = TargetReport {
            pid: 1,
            image: "/tmp/aft".to_string(),
            sha256: "test".to_string(),
            version: "test".to_string(),
            debug_id: "test".to_string(),
        };
        let profile = build_report(target, "test".to_string(), vec![inconsistent]);
        assert!(profile.top_symbols.is_empty());
        assert!(profile.threads[0].parse_inconsistent);
    }

    #[test]
    fn human_hot_paths_omit_runtime_prologue_and_cap_the_outer_frames() {
        let path = HotPath {
            samples: 10,
            frames: vec![
                "start  (in dyld) + 1".to_string(),
                "main  (in aft) + 1".to_string(),
                "std::sys::backtrace::__rust_begin_short_backtrace".to_string(),
                "std::thread::Thread::new::thread_start".to_string(),
                "aft::inspect::run".to_string(),
                "frame-1".to_string(),
                "frame-2".to_string(),
                "frame-3".to_string(),
                "frame-4".to_string(),
                "frame-5".to_string(),
                "frame-6".to_string(),
                "frame-7".to_string(),
                "frame-8".to_string(),
                "frame-9".to_string(),
                "frame-10".to_string(),
                "frame-11".to_string(),
                "frame-12".to_string(),
            ],
        };
        let rendered = human_hot_path_frames(&path);
        assert_eq!(rendered.first().map(String::as_str), Some("… (1 more)"));
        assert_eq!(rendered.last().map(String::as_str), Some("frame-12"));
        assert!(rendered.iter().all(|frame| !is_runtime_prologue(frame)));
        assert_eq!(rendered.len(), 13);
    }

    #[test]
    fn psynch_cvwait_is_not_running() {
        let thread = SampleThread {
            id: "9".to_string(),
            name: "worker".to_string(),
            declared_samples: 12,
            nodes: vec![SampleNode {
                parent: None,
                samples: 12,
                frame: "__psynch_cvwait (in libsystem_kernel.dylib) + 8".to_string(),
                offset: None,
            }],
        };
        let report = thread_report(&thread);
        assert_eq!(report.running_samples, 0);
        assert_eq!(report.waiting_samples, 12);
    }

    #[test]
    fn debug_file_selection_descends_into_extracted_dsym_bundles_only() {
        let cache = tempfile::tempdir().unwrap();
        let bundle = cache.path().join("aft.dSYM");
        let dwarf_dir = bundle.join("Contents/Resources/DWARF");
        fs::create_dir_all(&dwarf_dir).unwrap();
        let dwarf = dwarf_dir.join("aft-b9951e45f3d7609f");
        fs::write(&dwarf, b"DWARF").unwrap();
        fs::write(bundle.join("Contents/Info.plist"), b"plist").unwrap();

        assert_eq!(
            debug_file_in(cache.path(), Path::new("/tmp/ck-aft")),
            Some(dwarf)
        );
    }

    #[test]
    fn debug_file_selection_rejects_non_dwarf_bundle_files() {
        let cache = tempfile::tempdir().unwrap();
        let bundle = cache.path().join("aft.dSYM");
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(bundle.join("Contents/Info.plist"), b"plist").unwrap();

        assert_eq!(debug_file_in(cache.path(), Path::new("/tmp/ck-aft")), None);
    }

    #[test]
    fn debug_file_selection_accepts_linux_debug_layout() {
        let cache = tempfile::tempdir().unwrap();
        let layout = cache.path().join("aft.debug");
        fs::create_dir_all(&layout).unwrap();
        let debug = layout.join("aft.dwp");
        fs::write(&debug, b"debug").unwrap();

        assert_eq!(
            debug_file_in(cache.path(), Path::new("/tmp/aft")),
            Some(debug)
        );
    }

    #[test]
    fn debug_uuid_mismatch_refuses_fake_dwarf_path() {
        let error = validate_debug_artifact_uuid(
            "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA",
            "BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB",
            Path::new("/tmp/fake-other.dwarf"),
        )
        .expect_err("different fake DWARF UUIDs must not be accepted");
        assert!(error
            .to_string()
            .contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(error
            .to_string()
            .contains("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"));
        assert!(error.to_string().contains("fake-other.dwarf"));
    }

    #[test]
    fn detects_only_aft_subc_daemons() {
        let processes = parse_daemon_processes(
            " 101 /usr/local/bin/ck-aft /usr/local/bin/ck-aft --subc /tmp/one\n 102 /usr/bin/aft /usr/bin/aft --version\n 103 /usr/local/bin/aft /usr/local/bin/aft --subc=/tmp/two\n",
        );
        assert_eq!(
            processes
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![101, 103]
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn atos_symbolication_integration_uses_own_debug_binary_when_available() {
        eprintln!("SKIP: atos symbolication integration requires macOS");
    }

    #[cfg(target_os = "macos")]
    fn test_binary_text_load_address(binary: &Path) -> u64 {
        let output = Command::new("otool")
            .arg("-l")
            .arg(binary)
            .output()
            .expect("run otool for cargo test binary");
        assert!(
            output.status.success(),
            "otool must inspect the cargo test binary"
        );
        let text = String::from_utf8_lossy(&output.stdout);
        let lines = text.lines().collect::<Vec<_>>();
        let text_segment = lines
            .iter()
            .position(|line| line.trim() == "segname __TEXT")
            .expect("cargo test binary has a __TEXT segment");
        lines[text_segment..]
            .iter()
            .find_map(|line| line.trim().strip_prefix("vmaddr "))
            .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
            .expect("__TEXT segment has a hexadecimal vmaddr")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn atos_symbolication_integration_uses_own_debug_binary_when_available() {
        if which::which("atos").is_err() {
            eprintln!("SKIP: atos is not installed");
            return;
        }
        let binary = std::env::current_exe().expect("test binary path");
        if which::which("dsymutil").is_err() {
            eprintln!("SKIP: dsymutil is not installed");
            return;
        }
        let dsym = std::env::temp_dir().join(format!(
            "aft-profile-probe-{}-{}.dSYM",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        ));
        let status = Command::new("dsymutil")
            .arg(&binary)
            .arg("-o")
            .arg(&dsym)
            .status()
            .expect("start dsymutil for the cargo test binary");
        assert!(status.success(), "dsymutil must create a test-binary dSYM");
        let dwarf = debug_file_in(&dsym, &binary).expect("test binary dSYM contains DWARF");
        let _ = aft_profile_probe();
        unsafe extern "C" {
            fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
        }
        let slide = unsafe { _dyld_get_image_vmaddr_slide(0) };
        let load_address = (test_binary_text_load_address(&binary) as i128 + slide as i128) as u64;
        let offset = (aft_profile_probe as *const () as usize as u64)
            .checked_sub(load_address)
            .expect("profile probe belongs to the test binary __TEXT segment");
        let offset = format!("0x{offset:x}");
        let symbols = symbolicate_macos(&dwarf, &[offset.clone()])
            .expect("atos symbolicates the cargo test binary dSYM");
        assert!(
            symbols
                .get(&offset)
                .is_some_and(|symbol| symbol.contains("aft_profile_probe")),
            "atos must name aft_profile_probe, got {symbols:?}"
        );
    }
}
