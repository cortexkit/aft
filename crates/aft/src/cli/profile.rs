// The CPU sampler's parsers and symbolizers are inert on platforms without
// `sample`/`perf`; only the memory census view runs there, so the sampler's
// helpers are unreferenced by construction rather than dead.
#![cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]

use rustc_demangle::try_demangle;
use serde::Serialize;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use subc_client_rs::{CallOptions, CloseRouteOptions, ConsumerOptions};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};

use aft::commands::memory_census::MEMORY_CENSUS_OPERATION;
use aft::commands::writes_census::WRITES_CENSUS_OPERATION;

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
            Err(_) => {
                println!("AFT memory census unavailable: no daemon is connected.");
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
    if args.writes {
        let until_ms = epoch_ms();
        let since_ms = until_ms.saturating_sub(
            args.since_ms
                .unwrap_or(aft::write_ledger::DEFAULT_WINDOW_MS),
        );
        let root = args.root.as_ref().map(|path| {
            fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .display()
                .to_string()
        });
        // An omitted --root must omit the key: the daemon reads a JSON null as
        // an invalid root, not as "no filter".
        let mut params = serde_json::json!({ "since_ms": since_ms });
        if let Some(root) = root {
            params["root"] = serde_json::Value::String(root);
        }
        let before = fetch_management_census(WRITES_CENSUS_OPERATION, params.clone())
            .map_err(ProfileError::runtime)?;
        let census = if let Some(seconds) = args.live {
            std::thread::sleep(Duration::from_secs(seconds));
            let after = fetch_management_census(WRITES_CENSUS_OPERATION, params)
                .map_err(ProfileError::runtime)?;
            subtract_write_census(&after, &before, seconds.saturating_mul(1_000))
        } else {
            before
        };
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&census).map_err(|error| {
                    ProfileError::runtime(format!("could not render write census JSON: {error}"))
                })?
            );
        } else {
            print!("{}", render_writes_census_human(&census));
        }
        return Ok(());
    }
    if cfg!(target_os = "windows") {
        return Err(ProfileError::runtime(
            "aft profile unavailable: Windows CPU sampling is not supported",
        ));
    }

    let target = resolve_target(args.pid)?;
    let (captured, loaded_identity) = capture_profile_sample(&target, args.seconds)?;
    let mut threads = captured.threads;
    if threads.is_empty() {
        return Err(ProfileError::runtime(format!(
            "{} produced no thread samples for pid {}",
            captured.sampler, target.pid
        )));
    }
    let target_report = match inspect_target(&target, &captured.raw, loaded_identity.as_ref()) {
        Ok(report) => report,
        Err(error) => {
            let kept = preserve_raw_sample(target.pid, &captured.raw);
            return Err(ProfileError::runtime(match kept {
                Ok(path) => format!(
                    "{error}; the unsymbolicated sample was kept at {}",
                    path.display()
                ),
                Err(keep_error) => {
                    format!(
                        "{error}; the unsymbolicated sample could not be kept either: {keep_error}"
                    )
                }
            }));
        }
    };

    // Symbolization needs a debug artifact that may have to be fetched. If that
    // step fails, the captured sample is still the evidence the operator came
    // for: try to keep it on disk, name where it went (or why it could not be
    // kept), and still report the raw stacks with the reason on each affected
    // thread rather than discarding them.
    let symbolicated = resolve_debug_artifact(
        &target.image,
        &target_report.version,
        &target_report.debug_id,
        args.dsym.as_deref(),
    )
    .and_then(|debug| symbolicate(&mut threads, &debug, &target.image));
    match symbolicated {
        Err(error) => {
            let reason = match preserve_raw_sample(target.pid, &captured.raw) {
                Ok(path) => format!(
                    "{error}; the unsymbolicated sample was kept at {}",
                    path.display()
                ),
                Err(keep_error) => {
                    format!(
                        "{error}; the unsymbolicated sample could not be kept either: {keep_error}"
                    )
                }
            };
            eprintln!("aft profile: symbols unavailable: {reason}");
            mark_symbols_unavailable(&mut threads, &target.image, &reason);
        }
        Ok(()) => {
            if let Some(warning) = partial_symbols_warning(&threads) {
                eprintln!("{warning}");
            }
        }
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
    fetch_management_census(MEMORY_CENSUS_OPERATION, serde_json::json!({}))
}

fn fetch_management_census(
    operation: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let requested_operation = operation.to_owned();
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
        let catalog = consumer
            .catalog_list()
            .await
            .map_err(|error| format!("daemon catalog failed: {error}"))?;
        let advertises_census = catalog.modules.iter().any(|entry| {
            entry.module_id == "aft"
                && entry.roles.iter().any(|role| {
                    matches!(
                        role,
                        ProviderRole::ManagementSurface { operations, .. }
                            if operations.iter().any(|operation| {
                                operation.name == requested_operation
                            })
                    )
                })
        });
        if !advertises_census {
            return Err(format!(
                "daemon does not advertise AFT's {requested_operation}"
            ));
        }

        let project_root = std::env::current_dir()
            .map_err(|error| format!("could not resolve the profile working directory: {error}"))?;
        let route = consumer
            .open_route(
                RouteTarget::ManagementSurface {
                    module_id: "aft".to_string(),
                },
                BindIdentity::new(
                    project_root,
                    "aft-profile",
                    format!("aft-profile-{}", std::process::id()),
                ),
                CallOptions::default(),
            )
            .await
            .map_err(|error| format!("{requested_operation} route failed: {error}"))?;
        let body = serde_json::to_vec(&serde_json::json!({
            "op": requested_operation.clone(),
            "params": params,
        }))
        .map_err(|error| format!("could not encode {requested_operation} request: {error}"))?;
        let response = consumer
            .request(&route, body, CallOptions::default())
            .await
            .map_err(|error| format!("{requested_operation} request failed: {error}"));
        let _ = consumer
            .close_handle(&route, CloseRouteOptions::default())
            .await;
        let response = response?;
        let envelope: serde_json::Value = serde_json::from_slice(&response)
            .map_err(|error| format!("invalid {requested_operation} response: {error}"))?;
        if envelope.get("op").and_then(serde_json::Value::as_str)
            != Some(requested_operation.as_str())
            || envelope.get("status").and_then(serde_json::Value::as_str) != Some("ok")
        {
            // Carry the daemon's own verdict: a refusal with its code and
            // message is actionable, "unsuccessful" alone sends the reader
            // back to the daemon log.
            let status = envelope
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("missing");
            let detail = envelope
                .get("error")
                .map(|error| {
                    let code = error.get("code").and_then(serde_json::Value::as_str);
                    let message = error.get("message").and_then(serde_json::Value::as_str);
                    match (code, message) {
                        (Some(code), Some(message)) => format!("{code}: {message}"),
                        (Some(code), None) => code.to_string(),
                        (None, Some(message)) => message.to_string(),
                        (None, None) => error.to_string(),
                    }
                })
                .unwrap_or_else(|| envelope.to_string());
            return Err(format!(
                "daemon returned an unsuccessful {requested_operation} (status {status}): {detail}"
            ));
        }
        envelope
            .get("data")
            .filter(|data| data.is_object())
            .cloned()
            .ok_or_else(|| format!("{requested_operation} response did not contain object data"))
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
    writes: bool,
    since_ms: Option<u64>,
    root: Option<PathBuf>,
    live: Option<u64>,
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
            writes: false,
            since_ms: None,
            root: None,
            live: None,
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
                "--writes" => parsed.writes = true,
                "--since" => {
                    parsed.since_ms = Some(parse_duration(&next_value(&mut iter, "--since")?)?)
                }
                "--root" => parsed.root = Some(PathBuf::from(next_value(&mut iter, "--root")?)),
                "--live" => parsed.live = Some(parse_seconds(next_value(&mut iter, "--live")?)?),
                "--help" | "-h" => parsed.help = true,
                value if value.starts_with("--pid=") => {
                    parsed.pid = Some(parse_pid(value.trim_start_matches("--pid=").to_string())?)
                }
                value if value.starts_with("--seconds=") => {
                    parsed.seconds =
                        parse_seconds(value.trim_start_matches("--seconds=").to_string())?
                }
                value if value.starts_with("--since=") => {
                    parsed.since_ms = Some(parse_duration(value.trim_start_matches("--since="))?)
                }
                value if value.starts_with("--root=") => {
                    let path = value.trim_start_matches("--root=");
                    if path.is_empty() {
                        return Err(ProfileError::usage("--root requires a path"));
                    }
                    parsed.root = Some(PathBuf::from(path));
                }
                value if value.starts_with("--live=") => {
                    parsed.live = Some(parse_seconds(
                        value.trim_start_matches("--live=").to_string(),
                    )?)
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
        if parsed.memory && parsed.writes {
            return Err(ProfileError::usage(
                "--memory and --writes are mutually exclusive",
            ));
        }
        if !parsed.writes
            && (parsed.since_ms.is_some() || parsed.root.is_some() || parsed.live.is_some())
        {
            return Err(ProfileError::usage(
                "--since, --root, and --live require --writes",
            ));
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

fn parse_duration(value: &str) -> Result<u64, ProfileError> {
    let (number, multiplier) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1_000_u64),
        Some('m') => (&value[..value.len() - 1], 60_000_u64),
        Some('h') => (&value[..value.len() - 1], 60 * 60_000_u64),
        Some('d') => (&value[..value.len() - 1], 24 * 60 * 60_000_u64),
        Some(character) if character.is_ascii_digit() => (value, 1_000_u64),
        _ => {
            return Err(ProfileError::usage(
                "--since requires a positive duration such as 30s, 10m, 2h, or 1d",
            ))
        }
    };
    number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .and_then(|number| number.checked_mul(multiplier))
        .ok_or_else(|| {
            ProfileError::usage(format!(
                "--since requires a positive duration such as 30s, 10m, 2h, or 1d; got {value}"
            ))
        })
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn subtract_write_census(
    after: &serde_json::Value,
    before: &serde_json::Value,
    elapsed_ms: u64,
) -> serde_json::Value {
    let keyed = |value: &serde_json::Value| {
        value
            .get("writers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some((
                    (
                        row.get("domain")?.as_str()?.to_owned(),
                        row.get("root_id")?.as_str()?.to_owned(),
                    ),
                    (
                        row.get("logical_bytes")?.as_u64()?,
                        row.get("physical_bytes")?.as_u64()?,
                        row.get("seam_labels")
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                ))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before_rows = keyed(before);
    let mut writers = keyed(after)
        .into_iter()
        .filter_map(|((domain, root_id), (logical, physical, seam_labels))| {
            let previous = before_rows
                .get(&(domain.clone(), root_id.clone()))
                .cloned()
                .unwrap_or_default();
            let logical_bytes = logical.saturating_sub(previous.0);
            let physical_bytes = physical.saturating_sub(previous.1);
            (logical_bytes > 0 || physical_bytes > 0).then(|| {
                serde_json::json!({
                    "domain": domain,
                    "root_id": root_id,
                    "logical_bytes": logical_bytes,
                    "physical_bytes": physical_bytes,
                    "seam_labels": seam_labels,
                })
            })
        })
        .collect::<Vec<_>>();
    writers.sort_by(|left, right| {
        right["physical_bytes"]
            .as_u64()
            .cmp(&left["physical_bytes"].as_u64())
    });
    let attributed = writers.iter().fold(0_u64, |total, row| {
        total.saturating_add(row["physical_bytes"].as_u64().unwrap_or(0))
    });
    let delta_field = |field: &str| {
        after["process"][field]
            .as_u64()
            .zip(before["process"][field].as_u64())
            .map(|(after, before)| after.saturating_sub(before))
    };
    let keyed_unmeasurable = |value: &serde_json::Value| {
        value
            .get("unmeasurable")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some((
                    (
                        row.get("root_id")?.as_str()?.to_owned(),
                        row.get("seam")?.as_str()?.to_owned(),
                    ),
                    row.clone(),
                ))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before_unmeasurable = keyed_unmeasurable(before);
    let unmeasurable = keyed_unmeasurable(after)
        .into_iter()
        .filter_map(|((root_id, seam), row)| {
            let previous = before_unmeasurable.get(&(root_id.clone(), seam.clone()));
            let observations = row["observations"].as_u64().unwrap_or(0).saturating_sub(
                previous
                    .and_then(|value| value["observations"].as_u64())
                    .unwrap_or(0),
            );
            if observations == 0 {
                return None;
            }
            let estimated_physical_bytes = row["estimated_physical_bytes"].as_u64().map(|bytes| {
                bytes.saturating_sub(
                    previous
                        .and_then(|value| value["estimated_physical_bytes"].as_u64())
                        .unwrap_or(0),
                )
            });
            Some(serde_json::json!({
                "root_id": root_id,
                "seam": seam,
                "reason": row["reason"],
                "observations": observations,
                "estimated_physical_bytes": estimated_physical_bytes,
                "estimate_basis": row["estimate_basis"],
            }))
        })
        .collect::<Vec<_>>();
    let unmeasurable_estimate = unmeasurable.iter().fold(0_u64, |total, row| {
        total.saturating_add(row["estimated_physical_bytes"].as_u64().unwrap_or(0))
    });
    let process_physical = delta_field("physical_bytes");
    let classified = attributed.saturating_add(unmeasurable_estimate);
    serde_json::json!({
        "since_ms": after["until_ms"].as_u64().unwrap_or(0).saturating_sub(elapsed_ms),
        "until_ms": after["until_ms"],
        "root": after["root"],
        "writers": writers,
        "process": {
            "available": process_physical.is_some(),
            "logical_bytes": delta_field("logical_bytes"),
            "physical_bytes": process_physical,
        },
        "attributed_physical_bytes": attributed,
        "unmeasurable": unmeasurable,
        "unmeasurable_physical_bytes_estimate": unmeasurable_estimate,
        "unexplained_physical_bytes": process_physical.map(|physical| {
            let difference = i128::from(physical) - i128::from(classified);
            difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
        }),
        "coverage": after["coverage"],
        "live_elapsed_ms": elapsed_ms,
    })
}

pub fn render_writes_census_human(value: &serde_json::Value) -> String {
    let mut output = String::new();
    let process_physical = value["process"]["physical_bytes"].as_u64();
    let window_ms = value["until_ms"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(value["since_ms"].as_u64().unwrap_or(0));
    let _ = writeln!(output, "AFT writes ({:.1}m)", window_ms as f64 / 60_000.0);
    let _ = writeln!(
        output,
        "{:>12} {:>7} {:>12}  {:<24} root",
        "physical", "share", "logical", "domain"
    );
    if let Some(rows) = value.get("writers").and_then(serde_json::Value::as_array) {
        for row in rows {
            let physical = row["physical_bytes"].as_u64().unwrap_or(0);
            let logical = row["logical_bytes"].as_u64().unwrap_or(0);
            let share = process_physical
                .filter(|total| *total > 0)
                .map(|total| format!("{:.1}%", physical as f64 / total as f64 * 100.0))
                .unwrap_or_else(|| "n/a".to_owned());
            let _ = writeln!(
                output,
                "{:>12} {:>7} {:>12}  {:<24} {}",
                human_bytes(physical),
                share,
                human_bytes(logical),
                row["domain"].as_str().unwrap_or("other"),
                row["root_id"].as_str().unwrap_or("unknown")
            );
            if let Some(labels) = row.get("seam_labels").and_then(serde_json::Value::as_array) {
                if !labels.is_empty() {
                    let labels = labels
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(output, "             unmapped seams: {labels}");
                }
            }
        }
    }
    let attributed = value["attributed_physical_bytes"]
        .as_u64()
        .map(human_bytes)
        .unwrap_or_else(|| "unavailable".to_owned());
    let unmeasurable_estimate = value["unmeasurable_physical_bytes_estimate"]
        .as_u64()
        .map(human_bytes)
        .unwrap_or_else(|| "unavailable".to_owned());
    let unexplained = value["unexplained_physical_bytes"]
        .as_i64()
        .map(human_bytes_signed)
        .unwrap_or_else(|| "unavailable".to_owned());
    let unmeasurable = value
        .get("unmeasurable")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let without_estimate = unmeasurable
        .iter()
        .filter(|row| row["estimated_physical_bytes"].as_u64().is_none())
        .count();
    let _ = writeln!(output, "attributed physical: {attributed}");
    let suffix = if without_estimate == 0 {
        String::new()
    } else {
        format!("; {without_estimate} seam(s) have no byte estimate")
    };
    let _ = writeln!(
        output,
        "unmeasurable physical (estimate): {unmeasurable_estimate}{suffix}"
    );
    for row in unmeasurable {
        let seam = row["seam"].as_str().unwrap_or("unknown");
        let root = row["root_id"].as_str().unwrap_or("unknown");
        let reason = row["reason"].as_str().unwrap_or("reason unavailable");
        let estimate = row["estimated_physical_bytes"]
            .as_u64()
            .map(|bytes| format!("; estimate={}", human_bytes(bytes)))
            .unwrap_or_default();
        let basis = row["estimate_basis"]
            .as_str()
            .map(|basis| format!(" ({basis})"))
            .unwrap_or_default();
        let _ = writeln!(output, "  - {seam} [{root}]: {reason}{estimate}{basis}");
    }
    let _ = writeln!(output, "unexplained physical: {unexplained}");
    if value["coverage"]["complete"].as_bool() == Some(false) {
        let _ = writeln!(
            output,
            "coverage gap: requested window predates ledger by {}ms",
            value["coverage"]["gap_ms"].as_u64().unwrap_or(0)
        );
    }
    output
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024_f64 * 1024_f64 * 1024_f64))
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024_f64 * 1024_f64))
    } else if bytes >= 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024_f64)
    } else {
        format!("{bytes} B")
    }
}

fn human_bytes_signed(bytes: i64) -> String {
    if bytes < 0 {
        format!("-{}", human_bytes(bytes.unsigned_abs()))
    } else {
        human_bytes(bytes as u64)
    }
}

fn print_usage() {
    println!(
        "aft profile [--pid <pid>] [--seconds <N>] [--dsym <path>] [--json] [--raw] [--memory] [--writes [--since <duration>] [--root <path>] [--live <secs>]]"
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
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let image = fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| {
            ProfileError::runtime(format!(
                "could not resolve executable for pid {pid}: {error}"
            ))
        })?;
        // Linux appends this marker after an executable is unlinked. If the
        // executable's original path now exists, use it for disk inspection.
        if fs::metadata(&image).is_err() {
            if let Some(path) = image.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
                let current_path = PathBuf::from(OsString::from_vec(path.to_vec()));
                if fs::metadata(&current_path).is_ok() {
                    return Ok(current_path);
                }
            }
        }
        return Ok(image);
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
    image_on_disk_differs: bool,
    image_on_disk_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_on_disk_debug_id: Option<String>,
}

#[derive(Debug)]
struct TargetDebugIds {
    loaded: String,
    image_on_disk_differs: bool,
    image_on_disk: Option<String>,
    image_on_disk_status: String,
}

#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct LoadedImageIdentity {
    debug_id: String,
    device: u64,
    inode: u64,
    sha256: String,
    version: String,
}

fn select_debug_ids(
    loaded: &str,
    image_on_disk: Option<&str>,
    inode_differs: bool,
) -> TargetDebugIds {
    let differs = inode_differs
        || image_on_disk.is_none()
        || image_on_disk.is_some_and(|disk| normalize_debug_id(disk) != normalize_debug_id(loaded));
    TargetDebugIds {
        loaded: loaded.to_string(),
        image_on_disk_differs: differs,
        image_on_disk_status: if image_on_disk.is_some() {
            "readable".to_string()
        } else {
            "missing or unreadable".to_string()
        },
        image_on_disk: if differs {
            image_on_disk.map(str::to_string)
        } else {
            None
        },
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn binary_version(path: &Path) -> Result<String, ProfileError> {
    let version_output = command_text(Command::new(path).arg("--version"))?;
    Ok(version_output
        .split_whitespace()
        .find(|part| {
            part.chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
        })
        .unwrap_or("unknown")
        .trim()
        .to_string())
}

#[cfg(target_os = "macos")]
fn version_for_loaded_image(loaded_id: &str, disk_id: Option<&str>, image: &Path) -> String {
    version_for_loaded_image_with(loaded_id, disk_id, || binary_version(image))
}

// Used by the macOS loaded-image reader and by its tests on every platform.
#[cfg(any(target_os = "macos", test))]
fn version_for_loaded_image_with(
    loaded_id: &str,
    disk_id: Option<&str>,
    read_version: impl FnOnce() -> Result<String, ProfileError>,
) -> String {
    if disk_id.is_some_and(|disk| normalize_debug_id(disk) == normalize_debug_id(loaded_id)) {
        read_version().unwrap_or_else(|_| "unknown".to_string())
    } else {
        "unknown".to_string()
    }
}

fn image_on_disk_status(
    image: &Path,
    metadata: &Result<fs::Metadata, std::io::Error>,
    debug_id: Option<&str>,
) -> String {
    match metadata {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".to_string(),
        Err(error) => format!("unreadable: could not inspect {}: {error}", image.display()),
        Ok(_) if debug_id.is_none() => {
            format!(
                "unreadable: could not read debug ID from {}",
                image.display()
            )
        }
        Ok(_) => "readable".to_string(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_target(
    target: &Target,
    sample: &str,
    loaded_identity: Option<&LoadedImageIdentity>,
) -> Result<TargetReport, ProfileError> {
    #[cfg(target_os = "linux")]
    let (loaded_debug_id, disk_debug_id, inode_differs, disk_status) = {
        use std::os::unix::fs::MetadataExt;

        let _ = sample;
        let loaded = loaded_identity.ok_or_else(|| {
            ProfileError::runtime(format!(
                "loaded image identity was not captured before sampling pid {}",
                target.pid
            ))
        })?;
        let disk_metadata = fs::metadata(&target.image);
        let disk_debug_id = disk_metadata
            .as_ref()
            .ok()
            .and_then(|_| binary_debug_id(&target.image).ok());
        let inode_differs = disk_metadata.as_ref().map_or(true, |disk| {
            disk.dev() != loaded.device || disk.ino() != loaded.inode
        });
        let disk_status =
            image_on_disk_status(&target.image, &disk_metadata, disk_debug_id.as_deref());
        (
            loaded.debug_id.clone(),
            disk_debug_id,
            inode_differs,
            disk_status,
        )
    };
    #[cfg(target_os = "macos")]
    let (loaded_debug_id, disk_debug_id, inode_differs, disk_status) = {
        let _ = loaded_identity;
        let loaded_debug_id =
            parse_macos_main_image_uuid(sample, &target.image).ok_or_else(|| {
                ProfileError::runtime(format!(
                    "could not read loaded Mach-O UUID for {} from sample Binary Images",
                    target.image.display()
                ))
            })?;
        let disk_metadata = fs::metadata(&target.image);
        let disk_debug_id = disk_metadata
            .as_ref()
            .ok()
            .and_then(|_| binary_debug_id(&target.image).ok());
        let disk_status =
            image_on_disk_status(&target.image, &disk_metadata, disk_debug_id.as_deref());
        (loaded_debug_id, disk_debug_id, false, disk_status)
    };
    let mut debug_ids = select_debug_ids(&loaded_debug_id, disk_debug_id.as_deref(), inode_differs);
    debug_ids.image_on_disk_status = disk_status;
    #[cfg(target_os = "linux")]
    let (sha256, version) = {
        let loaded = loaded_identity.expect("Linux loaded image identity was required");
        (loaded.sha256.clone(), loaded.version.clone())
    };
    #[cfg(target_os = "macos")]
    let (sha256, version) = {
        let version =
            version_for_loaded_image(&loaded_debug_id, disk_debug_id.as_deref(), &target.image);
        // The sample gives the loaded UUID but not its bytes; hash only the
        // executable on disk when it is the same image that was sampled.
        ("unknown".to_string(), version)
    };
    Ok(TargetReport {
        pid: target.pid,
        image: target.image.display().to_string(),
        sha256,
        version,
        debug_id: debug_ids.loaded,
        image_on_disk_differs: debug_ids.image_on_disk_differs,
        image_on_disk_status: debug_ids.image_on_disk_status,
        image_on_disk_debug_id: debug_ids.image_on_disk,
    })
}

#[cfg(target_os = "linux")]
fn read_loaded_image_identity(pid: u32) -> Result<Option<LoadedImageIdentity>, ProfileError> {
    use std::os::unix::fs::MetadataExt;

    let path = PathBuf::from(format!("/proc/{pid}/exe"));
    match fs::read_link(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ProfileError::runtime(format!(
                "could not read loaded executable for pid {pid}: {error}"
            )))
        }
    }
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && !Path::new(&format!("/proc/{pid}")).exists() =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(ProfileError::runtime(format!(
                "could not inspect loaded executable for pid {pid}: {error}"
            )))
        }
    };
    let debug_id = match binary_debug_id(&path) {
        Ok(debug_id) => debug_id,
        Err(_) if !Path::new(&format!("/proc/{pid}")).exists() => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(LoadedImageIdentity {
        debug_id,
        device: metadata.dev(),
        inode: metadata.ino(),
        sha256: file_sha256(&path).unwrap_or_else(|_| "unknown".to_string()),
        version: binary_version(&path).unwrap_or_else(|_| "unknown".to_string()),
    }))
}

#[cfg(target_os = "linux")]
fn same_loaded_image(before: &LoadedImageIdentity, after: &LoadedImageIdentity) -> bool {
    before.device == after.device
        && before.inode == after.inode
        && normalize_debug_id(&before.debug_id) == normalize_debug_id(&after.debug_id)
}

#[cfg(target_os = "linux")]
fn capture_profile_sample(
    target: &Target,
    seconds: u64,
) -> Result<(CapturedSample, Option<LoadedImageIdentity>), ProfileError> {
    capture_with_image_identity(
        target.pid,
        || read_loaded_image_identity(target.pid),
        || capture_sample(target.pid, seconds),
    )
}

#[cfg(target_os = "linux")]
fn capture_with_image_identity(
    pid: u32,
    mut read_identity: impl FnMut() -> Result<Option<LoadedImageIdentity>, ProfileError>,
    capture: impl FnOnce() -> Result<CapturedSample, ProfileError>,
) -> Result<(CapturedSample, Option<LoadedImageIdentity>), ProfileError> {
    let before = read_identity()?.ok_or_else(|| {
        ProfileError::runtime(format!(
            "process {pid} exited before its loaded image could be recorded"
        ))
    })?;
    let captured = capture()?;
    match read_identity() {
        Ok(Some(after)) if !same_loaded_image(&before, &after) => Err(keep_sample_error(
            pid,
            &captured.raw,
            format!(
                "process {pid} exec'd during capture: loaded image changed from {} to {}; the sample cannot be attributed to either image",
                before.debug_id, after.debug_id
            ),
        )),
        Ok(Some(_)) | Ok(None) => Ok((captured, Some(before))),
        Err(error) => Err(keep_sample_error(
            pid,
            &captured.raw,
            format!("could not verify loaded image after sampling pid {pid}: {error}"),
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn capture_profile_sample(
    target: &Target,
    seconds: u64,
) -> Result<(CapturedSample, Option<LoadedImageIdentity>), ProfileError> {
    capture_sample(target.pid, seconds).map(|sample| (sample, None))
}

#[cfg(target_os = "linux")]
fn keep_sample_error(pid: u32, raw: &str, message: String) -> ProfileError {
    match preserve_raw_sample(pid, raw) {
        Ok(path) => ProfileError::runtime(format!(
            "{message}; the unsymbolicated sample was kept at {}",
            path.display()
        )),
        Err(error) => ProfileError::runtime(format!(
            "{message}; the unsymbolicated sample could not be kept either: {error}"
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_target(
    _target: &Target,
    _sample: &str,
    _loaded_identity: Option<&LoadedImageIdentity>,
) -> Result<TargetReport, ProfileError> {
    Err(ProfileError::runtime(
        "aft profile unavailable: loaded image inspection is not implemented on this platform",
    ))
}

#[cfg(target_os = "linux")]
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

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_macos_main_image_uuid(sample: &str, image: &Path) -> Option<String> {
    let image_name = image.file_name()?.to_string_lossy();
    let image_path = image.to_string_lossy();
    let mut in_binary_images = false;
    let mut basename_uuid = None;
    for line in sample.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Binary Images:") {
            in_binary_images = true;
            continue;
        }
        if !in_binary_images || trimmed.starts_with("Total number") {
            continue;
        }
        let uuid = trimmed
            .split_once('<')
            .and_then(|(_, tail)| tail.split_once('>'))
            .map(|(uuid, _)| normalize_debug_id(uuid));
        if trimmed.contains(image_path.as_ref()) {
            return uuid;
        }
        if trimmed.contains(image_name.as_ref()) && basename_uuid.is_none() {
            basename_uuid = uuid;
        }
    }
    basename_uuid
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
    aft::private_storage::write(&path, raw).map_err(|error| {
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
        aft::private_storage::create(&path)
            .map_err(|error| ProfileError::runtime(error.to_string()))?;
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
        aft::private_storage::create_dir_all(&directory).map_err(|error| {
            ProfileError::runtime(format!(
                "could not create perf temporary directory: {error}"
            ))
        })?;
        let perf_data = directory.join("perf.data");
        aft::private_storage::create(&perf_data)
            .map_err(|error| ProfileError::runtime(error.to_string()))?;
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
                symbols_unavailable: None,
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
    // `/proc/<tid>/stack` lists the innermost kernel frame first; store the
    // frames outermost first under the thread state, like the other samplers.
    let frames = stack
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    append_stack(
        thread,
        std::iter::once(state.to_string()).chain(frames.into_iter().rev()),
    );
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
                symbols_unavailable: None,
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

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_hex_address(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|part| part.starts_with("0x"))
        .map(|part| {
            part.trim_matches(|character: char| !character.is_ascii_hexdigit() && character != 'x')
        })
        .map(ToOwned::to_owned)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_perf_script(raw: &str) -> Vec<SampleThread> {
    let mut threads = BTreeMap::<String, SampleThread>::new();
    let mut current: Option<String> = None;
    // `perf script` prints each callchain innermost frame first. Collect a
    // whole sample, then store it outermost first so the tree has the same
    // orientation as macOS `sample` output (root = thread entry).
    let mut frames = Vec::<String>::new();
    let flush = |threads: &mut BTreeMap<String, SampleThread>,
                 current: &Option<String>,
                 frames: &mut Vec<String>| {
        if let Some(thread) = current.as_ref().and_then(|key| threads.get_mut(key)) {
            append_stack(thread, frames.drain(..).rev());
        }
        frames.clear();
    };
    for line in raw.lines() {
        if !line.starts_with(char::is_whitespace) {
            flush(&mut threads, &current, &mut frames);
            current = None;
            let Some((name, tid)) = parse_perf_header(line) else {
                continue;
            };
            let thread = threads.entry(tid.clone()).or_insert_with(|| SampleThread {
                id: tid.clone(),
                name,
                declared_samples: 0,
                nodes: Vec::new(),
                symbols_unavailable: None,
            });
            thread.declared_samples += 1;
            current = Some(tid);
            continue;
        }
        let text = line.trim();
        if current.is_some() && !text.is_empty() {
            frames.push(text.to_string());
        }
    }
    flush(&mut threads, &current, &mut frames);
    threads.into_values().collect()
}

/// Read the thread name and id from a `perf script` sample header. The
/// default fields print `comm tid [cpu] time: ...`; with `-F +pid` the id is
/// `pid/tid`, and `[cpu]` appears only when the samples carry a CPU. A thread
/// name may contain spaces and digits (`worker 7`), so the id is found from
/// the fixed fields after the name: the token just before the timestamp, or
/// before `[cpu]` when that is present. Reading left to right would take a
/// digit inside the name as the id and merge unrelated threads.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_perf_header(line: &str) -> Option<(String, String)> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    let is_number = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let is_timestamp = |text: &str| {
        text.strip_suffix(':').is_some_and(|value| {
            value.bytes().any(|byte| byte.is_ascii_digit())
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
    };
    let is_cpu = |text: &str| {
        text.strip_prefix('[')
            .and_then(|text| text.strip_suffix(']'))
            .is_some_and(is_number)
    };
    let time = fields.iter().position(|field| is_timestamp(field))?;
    let mut position = time.checked_sub(1)?;
    if is_cpu(fields[position]) {
        position = position.checked_sub(1)?;
    }
    // A header always starts with the thread name, so the id is never first.
    if position == 0 {
        return None;
    }
    let tid = match fields[position].rsplit_once('/') {
        Some((pid, tid)) if is_number(pid) && is_number(tid) => tid,
        None if is_number(fields[position]) => fields[position],
        _ => return None,
    };
    Some((fields[..position].join(" "), tid.to_string()))
}

/// Store one sample's frames, given outermost first, as a one-sample chain.
/// `thread_report` folds identical chains together before ranking.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn append_stack(thread: &mut SampleThread, frames: impl IntoIterator<Item = String>) {
    let mut parent = None;
    for frame in frames {
        let index = thread.nodes.len();
        thread.nodes.push(SampleNode {
            parent,
            samples: 1,
            offset: parse_hex_address(&frame),
            frame,
        });
        parent = Some(index);
    }
}

#[derive(Debug, Clone)]
struct SampleThread {
    id: String,
    name: String,
    declared_samples: u64,
    nodes: Vec<SampleNode>,
    /// Set when frames from the profiled image could not be symbolized; the
    /// raw frames stay in `nodes` and this says why.
    symbols_unavailable: Option<String>,
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
    let sibling = if cfg!(target_os = "macos") {
        PathBuf::from(format!("{}.dSYM", image.display()))
    } else {
        PathBuf::from(format!("{}.debug", image.display()))
    };
    let cache = debug_cache_dir(expected_id)?;
    resolve_debug_artifact_from(
        image,
        version,
        expected_id,
        supplied,
        &sibling,
        &cache,
        |candidate, explicit| inspect_debug_candidate(candidate, image, expected_id, explicit),
        download_release_debug,
    )
}

fn resolve_debug_artifact_from(
    image: &Path,
    version: &str,
    expected_id: &str,
    supplied: Option<&Path>,
    sibling: &Path,
    cache: &Path,
    mut inspect: impl FnMut(&Path, bool) -> Result<Option<DebugArtifact>, ProfileError>,
    download: impl FnOnce(&str, &str, &Path, &Path) -> Result<DebugArtifact, ProfileError>,
) -> Result<DebugArtifact, ProfileError> {
    if let Some(path) = supplied {
        return inspect(path, true)?.ok_or_else(|| {
            ProfileError::runtime(format!("debug UUID mismatch for {}", path.display()))
        });
    }

    match inspect(sibling, false) {
        Ok(Some(artifact)) => return Ok(artifact),
        Ok(None) => {}
        Err(error) if is_debug_uuid_mismatch(&error) => {}
        Err(error) => return Err(error),
    }
    match inspect(cache, false) {
        Ok(Some(artifact)) => return Ok(artifact),
        Ok(None) => {}
        Err(error) if is_debug_uuid_mismatch(&error) => {}
        Err(error) => return Err(error),
    }

    if version == "unknown" {
        return Err(ProfileError::runtime(format!(
            "no cached debug symbols for loaded UUID {}; stage the matching dSYM at {}",
            normalize_debug_id(expected_id),
            cache.display()
        )));
    }
    download(version, expected_id, image, cache)
}

fn is_debug_uuid_mismatch(error: &ProfileError) -> bool {
    error.message.starts_with("debug UUID mismatch for ")
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
    if !explicit && normalize_debug_id(expected_id) != normalize_debug_id(&actual_id) {
        return Ok(None);
    }
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
    image: &Path,
    cache: &Path,
) -> Result<DebugArtifact, ProfileError> {
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
    let cache_parent = cache.parent().ok_or_else(|| {
        ProfileError::runtime(format!("dSYM cache has no parent: {}", cache.display()))
    })?;
    aft::private_storage::create_dir_all(cache_parent).map_err(|error| {
        ProfileError::runtime(format!(
            "could not create dSYM cache root {}: {error}",
            cache_parent.display()
        ))
    })?;
    let download_dir = cache_parent.join(format!(
        ".download-{}-{}",
        normalize_debug_id(expected_id),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&download_dir);
    aft::private_storage::create_dir_all(&download_dir).map_err(|error| {
        ProfileError::runtime(format!(
            "could not create temporary dSYM directory {}: {error}",
            download_dir.display()
        ))
    })?;
    let archive = download_dir.join(&asset_name);
    aft::private_storage::write(&archive, bytes)
        .map_err(|error| ProfileError::runtime(format!("could not cache {asset_name}: {error}")))?;
    #[cfg(unix)]
    aft::private_storage::extract_zip(&archive, &download_dir).map_err(|error| {
        ProfileError::runtime(format!("could not extract {asset_name}: {error}"))
    })?;
    #[cfg(not(unix))]
    {
        let archive_path = archive.display().to_string();
        let cache_path = download_dir.display().to_string();
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
    }
    let _ = fs::remove_file(archive);
    match inspect_debug_candidate(&download_dir, image, expected_id, true) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = fs::remove_dir_all(&download_dir);
            return Err(ProfileError::runtime(format!(
                "downloaded debug artifact did not contain requested UUID/build-id {}",
                normalize_debug_id(expected_id)
            )));
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&download_dir);
            return Err(error);
        }
    }
    let _ = fs::remove_dir_all(cache);
    fs::rename(&download_dir, cache).map_err(|error| {
        ProfileError::runtime(format!(
            "could not install verified dSYM for UUID/build-id {} at {}: {error}",
            normalize_debug_id(expected_id),
            cache.display()
        ))
    })?;
    inspect_debug_candidate(cache, image, expected_id, true)?.ok_or_else(|| {
        ProfileError::runtime(format!(
            "verified debug artifact disappeared while installing UUID/build-id {}",
            normalize_debug_id(expected_id)
        ))
    })
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
        apply_symbols(threads, &symbols, image);
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let addresses = image_offsets(threads, image);
        let symbols = symbolicate_linux(&debug.dwarf, &addresses)?;
        apply_symbols(threads, &symbols, image);
        return Ok(());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (threads, debug, image);
        Ok(())
    }
}

/// Replace each image frame with its symbol. A frame the debug symbols could
/// not resolve keeps its raw `???` load-address text, and its thread records
/// how many such frames remain, so a partly symbolized stack never passes for
/// a fully symbolized one.
fn apply_symbols(threads: &mut [SampleThread], symbols: &HashMap<String, String>, image: &Path) {
    for thread in threads {
        let mut unresolved = 0_usize;
        for node in &mut thread.nodes {
            let image_offset = node
                .offset
                .as_ref()
                .filter(|_| frame_belongs_to_image(&node.frame, image));
            if let Some(offset) = image_offset {
                match symbols
                    .get(offset)
                    .filter(|symbol| is_resolved_symbol(symbol))
                {
                    Some(symbol) => node.frame = symbol.clone(),
                    None => unresolved += 1,
                }
            }
            node.frame = demangle_rust_symbols(&node.frame);
        }
        if unresolved > 0 {
            thread.symbols_unavailable = Some(format!(
                "{unresolved} frame(s) in {} did not resolve in the debug symbols; raw load-address frames kept",
                image_display_name(image)
            ));
        }
    }
}

/// `atos` echoes an unresolvable offset back as a hex address and `addr2line`
/// prints `??`; neither is a symbol.
fn is_resolved_symbol(symbol: &str) -> bool {
    let symbol = symbol.trim();
    !symbol.is_empty() && !symbol.starts_with("??") && !symbol.starts_with("0x")
}

/// One stderr line for a run where symbolization worked but left some frames
/// raw, so that a partial result is announced even when nobody reads each
/// thread's `symbols_unavailable`.
fn partial_symbols_warning(threads: &[SampleThread]) -> Option<String> {
    let affected = threads
        .iter()
        .filter(|thread| thread.symbols_unavailable.is_some())
        .count();
    (affected > 0).then(|| {
        format!(
            "aft profile: symbols unavailable for some frames on {affected} of {} thread(s); see symbols_unavailable on each affected thread",
            threads.len()
        )
    })
}

/// When no debug symbols could be used at all, every thread that has frames
/// from the profiled image keeps those frames raw and carries the reason.
fn mark_symbols_unavailable(threads: &mut [SampleThread], image: &Path, reason: &str) {
    for thread in threads {
        let has_image_frames = thread
            .nodes
            .iter()
            .any(|node| node.offset.is_some() && frame_belongs_to_image(&node.frame, image));
        if has_image_frames {
            thread.symbols_unavailable = Some(reason.to_string());
        }
    }
}

fn image_display_name(image: &Path) -> String {
    image
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| image.display().to_string())
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
    /// Why some of this thread's frames from the profiled image are still raw
    /// `???` load-address frames. Absent when every such frame was symbolized.
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols_unavailable: Option<String>,
}

#[derive(Debug, Serialize)]
struct HotPath {
    /// Running samples that passed through every frame in `frames`.
    samples: u64,
    frames: Vec<String>,
    /// True when the last frame in `frames` is not a leaf of the sample tree:
    /// the path stops there because at least a quarter of the thread's running
    /// samples go through that frame without going through any single callee
    /// that carries a quarter itself.
    truncated_at_fanout: bool,
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
    let merged = merge_matching_prefixes(thread);
    let thread = &merged;
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
    let heaviest_paths = heaviest_paths(thread, &running_paths, running_samples);
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
        symbols_unavailable: thread.symbols_unavailable.clone(),
    }
}

/// Rank where a thread's running samples go.
///
/// A frame qualifies when at least a quarter of the thread's running samples
/// pass through it. A qualifying frame is reported when at least a quarter of
/// the running samples pass through it but through none of its qualifying
/// direct callees: either a hot leaf (an exact stack) or a frame whose work is
/// spread over callees too small to qualify. Reporting only hot leaves would
/// leave a busy thread with an empty list whenever its work fans out, which
/// reads like "no work" when the work is in fact there. Those uncovered
/// quarters are disjoint, so at most four paths are reported. Each path's
/// `samples` is inclusive, so a fan-out path and a hot callee's path may nest
/// and their counts overlap. If no frame qualifies (the running samples are
/// split across several roots), the heaviest root is reported instead.
fn heaviest_paths(
    thread: &SampleThread,
    running_paths: &[(u64, Vec<usize>)],
    running_samples: u64,
) -> Vec<HotPath> {
    if running_samples == 0 {
        return Vec::new();
    }
    let mut inclusive = vec![0_u64; thread.nodes.len()];
    for (samples, path) in running_paths {
        for index in path {
            inclusive[*index] += samples;
        }
    }
    let threshold = running_samples.div_ceil(4);
    let mut has_children = vec![false; thread.nodes.len()];
    let mut in_qualifying_children = vec![0_u64; thread.nodes.len()];
    for (index, node) in thread.nodes.iter().enumerate() {
        if let Some(parent) = node.parent.filter(|parent| *parent < thread.nodes.len()) {
            has_children[parent] = true;
            if inclusive[index] >= threshold {
                in_qualifying_children[parent] += inclusive[index];
            }
        }
    }
    let hot_path = |index: usize| HotPath {
        samples: inclusive[index],
        frames: node_path(thread, index)
            .into_iter()
            .map(|index| thread.nodes[index].frame.clone())
            .collect(),
        truncated_at_fanout: has_children[index],
    };
    let mut paths = (0..thread.nodes.len())
        .filter(|index| {
            inclusive[*index] >= threshold
                && inclusive[*index].saturating_sub(in_qualifying_children[*index]) >= threshold
        })
        .map(hot_path)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        // Only reachable when the running samples are split across several
        // root frames, none holding a quarter. Name the heaviest root rather
        // than report nothing for a thread that ran.
        if let Some(root) = (0..thread.nodes.len())
            .filter(|index| thread.nodes[*index].parent.is_none() && inclusive[*index] > 0)
            .max_by_key(|index| inclusive[*index])
        {
            paths.push(hot_path(root));
        }
    }
    paths.sort_by(|left, right| right.samples.cmp(&left.samples));
    paths
}

/// Fold stacks that share the same frames from the root into one tree.
///
/// The Linux samplers record each sample as its own chain of nodes, while
/// macOS `sample` already prints a merged call tree. Ranking counts samples
/// per node, so without this step a stack seen 100 times on Linux would be 100
/// one-sample chains, and none would look hot. Folding makes both platforms
/// rank the same shape.
///
/// A node that ended a stack (a leaf) folds only into other leaves, never into
/// a frame that also has callees. Otherwise the samples spent in that frame
/// itself would disappear from the leaf totals that `parse_inconsistent` and
/// the running count are built from. Parents must precede their children in
/// `nodes`, which every parser here guarantees.
fn merge_matching_prefixes(thread: &SampleThread) -> SampleThread {
    let mut has_children = vec![false; thread.nodes.len()];
    for node in &thread.nodes {
        if let Some(parent) = node.parent.filter(|parent| *parent < thread.nodes.len()) {
            has_children[parent] = true;
        }
    }
    let mut merged = Vec::<SampleNode>::with_capacity(thread.nodes.len());
    let mut merged_index = vec![usize::MAX; thread.nodes.len()];
    let mut by_key = HashMap::<(Option<usize>, bool, &str), usize>::new();
    for (index, node) in thread.nodes.iter().enumerate() {
        // A parent that is missing or listed after its child cannot be
        // folded safely; keep such a node as its own root.
        let parent = node
            .parent
            .filter(|parent| *parent < index)
            .map(|parent| merged_index[parent]);
        let key = (parent, has_children[index], node.frame.as_str());
        let target = *by_key.entry(key).or_insert_with(|| {
            merged.push(SampleNode {
                parent,
                samples: 0,
                frame: node.frame.clone(),
                offset: node.offset.clone(),
            });
            merged.len() - 1
        });
        merged[target].samples += node.samples;
        merged_index[index] = target;
    }
    SampleThread {
        id: thread.id.clone(),
        name: thread.name.clone(),
        declared_samples: thread.declared_samples,
        nodes: merged,
        symbols_unavailable: thread.symbols_unavailable.clone(),
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
        "address-space slack (not resident memory; includes free pages already returned to the OS): {} MB",
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
    if let Some(io) = value
        .get("process_io")
        .or_else(|| value.get("process").and_then(|p| p.get("process_io")))
    {
        if io.get("available").and_then(serde_json::Value::as_bool) == Some(true) {
            let read = io
                .get("diskio_bytes_read")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let written = io
                .get("diskio_bytes_written")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let logical = io
                .get("logical_bytes_written")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            writeln!(
                &mut output,
                "process io: read {} GB, written {} GB (logical {} GB) since spawn",
                gb(read),
                gb(written),
                gb(logical),
            )
            .unwrap();
        } else {
            writeln!(&mut output, "process io: unavailable").unwrap();
        }
    }
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
fn gb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}
fn mb_signed(bytes: i64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn print_human(report: &ProfileReport) {
    print!("{}", render_human(report));
}

fn render_human(report: &ProfileReport) -> String {
    let mut out = String::new();
    writeln!(out, "AFT CPU profile ({})", report.sampler).unwrap();
    writeln!(out, "pid: {}", report.target.pid).unwrap();
    writeln!(out, "image: {}", report.target.image).unwrap();
    writeln!(out, "sha256: {}", report.target.sha256).unwrap();
    writeln!(out, "version: {}", report.target.version).unwrap();
    out.push_str(&render_target_identity(&report.target));
    writeln!(out, "\nThread census (running / total):").unwrap();
    let mut threads = report.threads.iter().collect::<Vec<_>>();
    threads.sort_by(|left, right| right.running_samples.cmp(&left.running_samples));
    for thread in threads {
        if thread.parse_inconsistent {
            writeln!(
                out,
                "  {} {}: (parse inconsistent: leaves={} declared={}) — {}",
                thread.id, thread.name, thread.leaf_samples, thread.total_samples, thread.verdict
            )
            .unwrap();
        } else {
            writeln!(
                out,
                "  {} {}: {} / {} running ({} waiting) — {}",
                thread.id,
                thread.name,
                thread.running_samples,
                thread.total_samples,
                thread.waiting_samples,
                thread.verdict
            )
            .unwrap();
        }
        // Shown on every affected thread, waiting or below the hot cutoff
        // too, so a raw stack is never mistaken for a symbolized one.
        if let Some(reason) = &thread.symbols_unavailable {
            writeln!(out, "    {} symbols unavailable: {reason}", thread.id).unwrap();
        }
    }
    writeln!(out, "\nHot running threads:").unwrap();
    for thread in report
        .threads
        .iter()
        .filter(|thread| !thread.parse_inconsistent && thread.running_samples > 0)
        .take(TOP_THREADS)
    {
        for path in &thread.heaviest_paths {
            writeln!(
                out,
                "  {} ({}): {}{}",
                thread.id,
                path.samples,
                human_hot_path_frames(path).join(" > "),
                if path.truncated_at_fanout {
                    " > (spread over smaller callees)"
                } else {
                    ""
                }
            )
            .unwrap();
        }
    }
    writeln!(out, "\nTop inclusive running symbols:").unwrap();
    for symbol in &report.top_symbols {
        writeln!(out, "  {:>5} {}", symbol.inclusive_samples, symbol.symbol).unwrap();
    }
    if let Some(raw) = &report.raw_sample {
        writeln!(out, "\nRaw sampler output:\n{raw}").unwrap();
    }
    out
}

fn render_target_identity(target: &TargetReport) -> String {
    let mut rendered = format!(
        "loaded debug ID: {}\nimage_on_disk_status: {}\nimage_on_disk_differs: {}\n",
        target.debug_id, target.image_on_disk_status, target.image_on_disk_differs
    );
    if target.image_on_disk_differs {
        if let Some(debug_id) = &target.image_on_disk_debug_id {
            rendered.push_str(&format!("image_on_disk_debug_id: {debug_id}\n"));
        }
        rendered.push_str("symbols are from the loaded image\n");
    }
    rendered
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
    fn writes_profile_args_and_renderer_keep_filters_gap_and_residual_split() {
        let parsed = ProfileArgs::parse(vec![
            OsString::from("--writes"),
            OsString::from("--since"),
            OsString::from("10m"),
            OsString::from("--root=/repo"),
            OsString::from("--live=2"),
        ])
        .unwrap();
        assert!(parsed.writes);
        assert_eq!(parsed.since_ms, Some(600_000));
        assert_eq!(parsed.root, Some(PathBuf::from("/repo")));
        assert_eq!(parsed.live, Some(2));

        let rendered = render_writes_census_human(&serde_json::json!({
            "since_ms": 0,
            "until_ms": 600_000,
            "writers": [{
                "domain": "semantic_delta",
                "root_id": "/repo",
                "logical_bytes": 4096,
                "physical_bytes": 1024,
                "seam_labels": []
            }],
            "process": {"available": true, "logical_bytes": 8192, "physical_bytes": 4096},
            "attributed_physical_bytes": 1024,
            "unmeasurable": [{
                "root_id": "/repo",
                "seam": "db::TrackedConnection::drop",
                "reason": "reopening the live SQLite file set would release POSIX locks",
                "observations": 1,
                "estimated_physical_bytes": 2048,
                "estimate_basis": "outstanding WAL frames times page size"
            }],
            "unmeasurable_physical_bytes_estimate": 2048,
            "unexplained_physical_bytes": 1024,
            "coverage": {"complete": false, "gap_ms": 3000}
        }));
        assert!(rendered.contains("semantic_delta"));
        assert!(rendered.contains("25.0%"));
        assert!(rendered.contains("attributed physical: 1.00 KiB"));
        assert!(rendered.contains("unmeasurable physical (estimate): 2.00 KiB"));
        assert!(rendered.contains("db::TrackedConnection::drop"));
        assert!(rendered.contains("unexplained physical: 1.00 KiB"));
        assert!(rendered.contains("coverage gap"));
    }

    #[test]
    fn live_write_profile_subtracts_the_unmeasurable_estimate_from_unexplained() {
        let census = |until_ms, physical, attributed, observations, estimate| {
            serde_json::json!({
                "since_ms": 0,
                "until_ms": until_ms,
                "root": null,
                "writers": [{
                    "domain": "callgraph_refresh",
                    "root_id": "/repo",
                    "logical_bytes": attributed,
                    "physical_bytes": attributed,
                    "seam_labels": []
                }],
                "process": {
                    "available": true,
                    "logical_bytes": physical,
                    "physical_bytes": physical
                },
                "unmeasurable": [{
                    "root_id": "/repo",
                    "seam": "db::TrackedConnection::drop",
                    "reason": "close result unavailable",
                    "observations": observations,
                    "estimated_physical_bytes": estimate,
                    "estimate_basis": "outstanding frames"
                }],
                "coverage": {"complete": true, "gap_ms": 0}
            })
        };
        let before = census(1_000, 200, 100, 1, 50);
        let after = census(3_000, 260, 140, 2, 70);

        let live = subtract_write_census(&after, &before, 2_000);
        assert_eq!(live["attributed_physical_bytes"], 40);
        assert_eq!(live["unmeasurable_physical_bytes_estimate"], 20);
        assert_eq!(live["unexplained_physical_bytes"], 0);
        assert_eq!(live["unmeasurable"][0]["observations"], 1);
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
    fn render_memory_census_human_renders_process_io_line_and_updated_slack_label() {
        let value = serde_json::json!({
            "process": {
                "phys_footprint_bytes": 100 * 1024 * 1024,
                "rss_bytes": 80 * 1024 * 1024,
                "allocator_slack_bytes": 20 * 1024 * 1024,
                "sqlite_bytes": 5 * 1024 * 1024,
                "total_attributed_bytes": 50 * 1024 * 1024,
                "unattributed_bytes": 30 * 1024 * 1024,
            },
            "process_io": {
                "available": true,
                "sampled_at_ms": 1_000,
                "diskio_bytes_read": 1024_u64 * 1024 * 1024,
                "diskio_bytes_written": 2_u64 * 1024 * 1024 * 1024,
                "logical_bytes_written": 3_u64 * 1024 * 1024 * 1024,
            },
            "roots": {}
        });
        let rendered = render_memory_census_human(&value);
        assert!(rendered.contains("address-space slack (not resident memory; includes free pages already returned to the OS): 20.0 MB\n"));
        assert!(rendered
            .contains("process io: read 1.0 GB, written 2.0 GB (logical 3.0 GB) since spawn\n"));
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
            image_on_disk_differs: false,
            image_on_disk_status: "readable".to_string(),
            image_on_disk_debug_id: None,
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
    fn profile_uses_loaded_uuid_and_reports_replaced_disk_image() {
        let loaded_uuid = "11111111-2222-3333-4444-555555555555";
        let disk_uuid = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        let debug_ids = select_debug_ids(loaded_uuid, Some(disk_uuid), false);
        assert_eq!(debug_ids.loaded, loaded_uuid);
        assert!(debug_ids.image_on_disk_differs);
        assert_eq!(debug_ids.image_on_disk.as_deref(), Some(disk_uuid));

        let same_uuid_on_replaced_inode = select_debug_ids(loaded_uuid, Some(loaded_uuid), true);
        assert!(same_uuid_on_replaced_inode.image_on_disk_differs);
        assert_eq!(
            same_uuid_on_replaced_inode.image_on_disk.as_deref(),
            Some(loaded_uuid)
        );

        let report = TargetReport {
            pid: 17,
            image: "/opt/aft/daemon".to_string(),
            sha256: "unknown".to_string(),
            version: "unknown".to_string(),
            debug_id: debug_ids.loaded,
            image_on_disk_differs: debug_ids.image_on_disk_differs,
            image_on_disk_status: debug_ids.image_on_disk_status,
            image_on_disk_debug_id: debug_ids.image_on_disk,
        };
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["uuid_or_build_id"], loaded_uuid);
        assert_eq!(json["image_on_disk_differs"], true);
        assert_eq!(json["image_on_disk_debug_id"], disk_uuid);
        assert_eq!(json["image_on_disk_status"], "readable");
    }

    #[test]
    fn mismatched_implicit_sibling_falls_through_to_cached_debug_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("aft");
        let sibling = temp.path().join("aft.debug");
        let cache = temp.path().join("dsym/LOADED");
        let cached_dwarf = cache.join("loaded.debug");
        let mut inspected = Vec::new();

        let resolved = resolve_debug_artifact_from(
            &image,
            "1.2.3",
            "LOADED",
            None,
            &sibling,
            &cache,
            |candidate, explicit| {
                inspected.push((candidate.to_path_buf(), explicit));
                if candidate == sibling {
                    Err(ProfileError::runtime(format!(
                        "debug UUID mismatch for {}: sibling has another UUID",
                        sibling.display()
                    )))
                } else if candidate == cache {
                    Ok(Some(DebugArtifact {
                        dwarf: cached_dwarf.clone(),
                    }))
                } else {
                    Ok(None)
                }
            },
            |_, _, _, _| panic!("release download must not run after a cache hit"),
        )
        .expect("the cache has symbols for the loaded image");

        assert_eq!(resolved.dwarf, cached_dwarf);
        assert_eq!(inspected, vec![(sibling, false), (cache, false)]);
    }

    #[test]
    fn mismatched_explicit_dsym_is_refused_without_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("aft");
        let sibling = temp.path().join("aft.debug");
        let cache = temp.path().join("dsym/LOADED");
        let supplied = temp.path().join("operator.dsym");
        let mut inspected = Vec::new();

        let error = resolve_debug_artifact_from(
            &image,
            "1.2.3",
            "LOADED",
            Some(&supplied),
            &sibling,
            &cache,
            |candidate, explicit| {
                inspected.push((candidate.to_path_buf(), explicit));
                Err(ProfileError::runtime(format!(
                    "debug UUID mismatch for {}: wrong UUID",
                    candidate.display()
                )))
            },
            |_, _, _, _| panic!("an explicit mismatch must not fall back to a download"),
        )
        .expect_err("an explicitly selected artifact with a different UUID is unsafe");

        assert!(error.to_string().contains("debug UUID mismatch"));
        assert_eq!(inspected, vec![(supplied, true)]);
    }

    #[test]
    fn human_profile_output_names_loaded_and_replaced_disk_images() {
        let target = TargetReport {
            pid: 17,
            image: "/opt/aft/daemon".to_string(),
            sha256: "unknown".to_string(),
            version: "unknown".to_string(),
            debug_id: "LOADED-UUID".to_string(),
            image_on_disk_differs: true,
            image_on_disk_status: "readable".to_string(),
            image_on_disk_debug_id: Some("DISK-UUID".to_string()),
        };
        let rendered = render_target_identity(&target);
        assert!(rendered.contains("loaded debug ID: LOADED-UUID"));
        assert!(rendered.contains("image_on_disk_differs: true"));
        assert!(rendered.contains("image_on_disk_debug_id: DISK-UUID"));
        assert!(rendered.contains("symbols are from the loaded image"));
    }

    #[test]
    fn matching_loaded_uuid_uses_disk_version_for_release_lookup() {
        let image = Path::new("/opt/aft/daemon");
        let loaded_id = "1111-2222";
        let version =
            version_for_loaded_image_with(loaded_id, Some("11112222"), || Ok("8.2.1".to_string()));
        assert_eq!(version, "8.2.1");

        let sibling = Path::new("/missing/aft.debug");
        let cache = Path::new("/missing/cache");
        let artifact = resolve_debug_artifact_from(
            image,
            &version,
            loaded_id,
            None,
            sibling,
            cache,
            |_, _| Ok(None),
            |release_version, _, _, _| {
                assert_eq!(release_version, "8.2.1");
                Ok(DebugArtifact {
                    dwarf: PathBuf::from("release-debug-file"),
                })
            },
        )
        .expect("the matching loaded image has a release version");
        assert_eq!(artifact.dwarf, PathBuf::from("release-debug-file"));
    }

    #[test]
    fn differing_loaded_uuid_without_cache_skips_release_and_names_staging_path() {
        let version = version_for_loaded_image_with("LOADED-UUID", Some("DISK-UUID"), || {
            panic!("a replaced disk image must not be used to guess the loaded version")
        });
        assert_eq!(version, "unknown");

        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("dsym/LOADEDUUID");
        let error = resolve_debug_artifact_from(
            Path::new("/opt/aft/daemon"),
            &version,
            "LOADED-UUID",
            None,
            Path::new("/missing/aft.debug"),
            &cache,
            |_, _| Ok(None),
            |_, _, _, _| panic!("unknown loaded version must not trigger a release request"),
        )
        .expect_err("without cached symbols the loaded image cannot be symbolicated");
        let message = error.to_string();
        assert!(message.contains("LOADEDUUID"));
        assert!(message.contains("stage the matching dSYM"));
        assert!(message.contains(&cache.display().to_string()));
    }

    #[test]
    fn unreadable_disk_image_is_not_reported_as_a_match() {
        let loaded_id = "LOADED-UUID";
        let missing = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "image was removed",
        ));
        let status = image_on_disk_status(Path::new("/opt/aft/daemon"), &missing, None);
        let ids = select_debug_ids(loaded_id, None, false);
        assert_eq!(status, "missing");
        assert!(ids.image_on_disk_differs);

        let target = TargetReport {
            pid: 17,
            image: "/opt/aft/daemon".to_string(),
            sha256: "unknown".to_string(),
            version: "unknown".to_string(),
            debug_id: loaded_id.to_string(),
            image_on_disk_differs: ids.image_on_disk_differs,
            image_on_disk_status: status,
            image_on_disk_debug_id: None,
        };
        let rendered = render_target_identity(&target);
        assert!(rendered.contains("image_on_disk_status: missing"));
        assert!(rendered.contains("symbols are from the loaded image"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_capture_keeps_pre_capture_identity_if_process_exits() {
        let before = LoadedImageIdentity {
            debug_id: "BEFORE".to_string(),
            device: 1,
            inode: 2,
            sha256: "hash".to_string(),
            version: "1.0".to_string(),
        };
        let mut identities = [Some(before.clone()), None].into_iter();
        let (captured, identity) = capture_with_image_identity(
            424240,
            || Ok(identities.next().expect("pre and post identity reads")),
            || {
                Ok(CapturedSample {
                    sampler: "test".to_string(),
                    raw: "sample captured before exit".to_string(),
                    threads: vec![],
                })
            },
        )
        .expect("a process exit after capture does not invalidate its sample");

        assert_eq!(captured.raw, "sample captured before exit");
        assert_eq!(identity.unwrap().debug_id, "BEFORE");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_capture_refuses_to_attribute_samples_after_exec() {
        let identity = |debug_id: &str, inode| LoadedImageIdentity {
            debug_id: debug_id.to_string(),
            device: 1,
            inode,
            sha256: "hash".to_string(),
            version: "1.0".to_string(),
        };
        let mut identities = [Some(identity("BEFORE", 2)), Some(identity("AFTER", 3))].into_iter();
        let error = capture_with_image_identity(
            424241,
            || Ok(identities.next().expect("pre and post identity reads")),
            || {
                Ok(CapturedSample {
                    sampler: "test".to_string(),
                    raw: "sample captured before exec".to_string(),
                    threads: vec![],
                })
            },
        )
        .expect_err("samples from the old image must not be attributed to the new image");

        let message = error.to_string();
        assert!(message.contains("exec'd during capture"));
        assert!(message.contains("BEFORE"));
        assert!(message.contains("AFTER"));
        assert!(message.contains("unsymbolicated sample was kept"));
        let kept_path = message
            .split("the unsymbolicated sample was kept at ")
            .nth(1)
            .expect("error names preserved sample path");
        let _ = fs::remove_file(kept_path);
    }

    #[test]
    fn parses_loaded_macos_image_uuid_from_binary_images() {
        let sample = "Binary Images:\n  0x1000 - 0x2000 + aft (arm64) <11111111-2222-3333-4444-555555555555> /opt/aft/daemon\n";
        assert_eq!(
            parse_macos_main_image_uuid(sample, Path::new("/opt/aft/daemon")),
            Some("11111111222233334444555555555555".to_string())
        );
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

    // A live `sample` of the daemon's health-rollup thread (unsymbolicated, with
    // load addresses). Its working samples sit under one frame and then spread
    // across many leaves of 1-9 samples each, so no single leaf stack reaches a
    // quarter of the running samples. The report must still name where the work
    // is instead of returning an empty path list.
    #[test]
    fn busy_thread_whose_work_fans_out_still_reports_heaviest_path() {
        let raw = include_str!("../../tests/fixtures/ck-aft-health-rollup-sample.txt");
        let threads = parse_macos_sample(raw);
        assert_eq!(threads.len(), 1);
        let report = thread_report(&threads[0]);
        assert_eq!(report.name, "aft-health-rollup");
        assert!(!report.parse_inconsistent);
        assert_eq!(report.total_samples, 526);
        assert_eq!(report.waiting_samples, 385);
        assert_eq!(report.running_samples, 141);

        assert!(
            !report.heaviest_paths.is_empty(),
            "a thread with {} running samples reported no heaviest path",
            report.running_samples
        );
        let heaviest = &report.heaviest_paths[0];
        assert_eq!(heaviest.samples, 140);
        assert_eq!(heaviest.frames.len(), 5);
        assert!(heaviest.frames[0].starts_with("thread_start"));
        assert!(heaviest.frames[4].contains("load address 0x1043fc000 + 0x955a58"));
        assert!(heaviest.truncated_at_fanout);
        // The two callees that each carry a quarter of the running samples are
        // reported too, nested under the fan-out frame. The 36-sample one ends
        // one frame deeper, at its only callee, which carries all 36.
        let nested = report.heaviest_paths[1..]
            .iter()
            .map(|path| (path.samples, path.frames.len()))
            .collect::<Vec<_>>();
        assert_eq!(nested, vec![(37, 6), (36, 7)]);
        let threshold = report.running_samples.div_ceil(4);
        assert!(report
            .heaviest_paths
            .iter()
            .all(|path| path.samples >= threshold));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["heaviest_paths"][0]["samples"], 140);
        assert_eq!(json["heaviest_paths"][0]["truncated_at_fanout"], true);
        assert!(json.get("symbols_unavailable").is_none());
    }

    // `perf script` default text: one callchain per sample, innermost frame
    // first. The health-rollup thread repeats one stack 100 times and spreads
    // another 60 samples under one frame over 30 leaf sites; the executor
    // thread uses the `pid/tid` header that `perf script -F +pid` prints.
    // A thread name may itself contain numbers; the id comes from the fixed
    // fields after the name, so `worker 7` with tid 4242 is thread 4242.
    #[test]
    fn perf_header_takes_the_id_after_a_name_that_contains_numbers() {
        let cases = [
            (
                "worker 7 4242 [003] 12345.004000: 1 cycles:",
                "worker 7",
                "4242",
            ),
            ("worker 7 4242 12345.004000: 1 cycles:", "worker 7", "4242"),
            ("pool 1/2 99/4243 [000] 1.5: 1 cycles:", "pool 1/2", "4243"),
            (
                "aft-health-rollup 4244 77.25: 1 cycles:",
                "aft-health-rollup",
                "4244",
            ),
        ];
        for (header, name, tid) in cases {
            assert_eq!(
                parse_perf_header(header),
                Some((name.to_string(), tid.to_string())),
                "{header}"
            );
        }
        // Stack frames and lines without a timestamp are not headers.
        assert_eq!(
            parse_perf_header("55d1c0a2d0b3 aft::subc::health::run+0x23 (/x)"),
            None
        );
        assert_eq!(parse_perf_header("4242 12345.004000: 1 cycles:"), None);
    }

    #[test]
    fn linux_perf_stacks_fold_into_one_tree_before_ranking() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/linux-perf-script.json"))
                .unwrap();
        let mut raw = String::new();
        let mut sample = 0;
        let mut append = |frame: &str| {
            sample += 1;
            raw.push_str(&frame.replace("@TIME@", &format!("12345.{:06}", sample * 4000)));
        };
        // The original trace alternates five actor samples and three scan
        // samples. Thirty distinct scan leaves recur twice to exercise fanout.
        for cycle in 0..20 {
            for _ in 0..5 {
                append(fixture["actorSample"].as_str().unwrap());
            }
            for leaf in (cycle * 3)..(cycle * 3 + 3) {
                let leaf = leaf % 30;
                append(
                    &fixture["scanSample"]
                        .as_str()
                        .unwrap()
                        .replace("@ADDR@", &format!("{:x}", 0x55d1c0b00000u64 + leaf))
                        .replace("@OFFSET@", &format!("{:x}", leaf * 4)),
                );
            }
        }
        for _ in 0..8 {
            append(fixture["executorSample"].as_str().unwrap());
        }
        // Pin the full original bytes, not just the parsed stack counts.
        use sha2::{Digest, Sha256};
        assert_eq!(
            format!("{:x}", Sha256::digest(raw.as_bytes())),
            fixture["sha256"].as_str().unwrap()
        );
        let threads = parse_perf_script(&raw);
        let ids = threads
            .iter()
            .map(|thread| (thread.id.as_str(), thread.declared_samples))
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![("4242", 160), ("4243", 8)]);
        assert_eq!(threads[0].name, "aft-health-roll");

        let report = thread_report(&threads[0]);
        assert!(!report.parse_inconsistent);
        assert_eq!(report.running_samples, 160);
        let summary = report
            .heaviest_paths
            .iter()
            .map(|path| {
                (
                    path.samples,
                    path.truncated_at_fanout,
                    path.frames.first().cloned().unwrap_or_default(),
                    path.frames.last().cloned().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        let outermost = "7f3a2b126850 __clone3+0x30 (/usr/lib/x86_64-linux-gnu/libc.so.6)";
        assert_eq!(
            summary,
            vec![
                (
                    100,
                    false,
                    outermost.to_string(),
                    "55d1c0a2d0b3 aft::subc::health::count_actors+0x23 (/usr/local/bin/aft)"
                        .to_string()
                ),
                (
                    60,
                    true,
                    outermost.to_string(),
                    "55d1c0a2e2c8 aft::subc::health::scan_actor_snapshots+0x88 (/usr/local/bin/aft)"
                        .to_string()
                ),
            ]
        );

        let executor = thread_report(&threads[1]);
        assert_eq!(executor.heaviest_paths.len(), 1);
        assert_eq!(executor.heaviest_paths[0].samples, 8);
        assert!(!executor.heaviest_paths[0].truncated_at_fanout);
    }

    #[test]
    fn folding_keeps_samples_that_ended_in_a_frame_with_callees_elsewhere() {
        // Three perf samples: one stopped in `B` itself, two went on into `C`.
        let mut thread = SampleThread {
            id: "7".to_string(),
            name: "worker".to_string(),
            declared_samples: 3,
            nodes: Vec::new(),
            symbols_unavailable: None,
        };
        for frames in [vec!["A", "B"], vec!["A", "B", "C"], vec!["A", "B", "C"]] {
            append_stack(&mut thread, frames.into_iter().map(str::to_string));
        }
        let report = thread_report(&thread);
        assert!(!report.parse_inconsistent);
        assert_eq!(report.leaf_samples, 3);
        assert_eq!(report.running_samples, 3);
    }

    #[test]
    fn symbol_gaps_are_shown_on_every_affected_thread_past_the_hot_cutoff() {
        let thread = |id: &str, samples: u64, frame: &str, reason: Option<&str>| SampleThread {
            id: id.to_string(),
            name: format!("worker-{id}"),
            declared_samples: samples,
            nodes: vec![SampleNode {
                parent: None,
                samples,
                frame: frame.to_string(),
                offset: None,
            }],
            symbols_unavailable: reason.map(str::to_string),
        };
        let busy = "???  (in ck-aft)  load address 0x1000 + 0x10";
        let parked = "__psynch_cvwait  (in libsystem_kernel.dylib) + 8";
        let past_cutoff = "3 frame(s) in ck-aft did not resolve in the debug symbols";
        let waiting = "no cached debug symbols for loaded UUID ABC";
        let mut threads = (1..=TOP_THREADS as u64)
            .map(|rank| thread(&format!("hot{rank}"), 100 - rank, busy, None))
            .collect::<Vec<_>>();
        // Ranked sixth by running samples, so it is not among the hot threads.
        threads.push(thread("cool", 5, busy, Some(past_cutoff)));
        threads.push(thread("parked", 50, parked, Some(waiting)));
        let target = TargetReport {
            pid: 1,
            image: "/tmp/ck-aft".to_string(),
            sha256: "test".to_string(),
            version: "test".to_string(),
            debug_id: "test".to_string(),
            image_on_disk_differs: false,
            image_on_disk_status: "readable".to_string(),
            image_on_disk_debug_id: None,
        };
        let report = build_report(target, "test".to_string(), threads);

        let human = render_human(&report);
        assert!(
            human.contains(&format!("cool symbols unavailable: {past_cutoff}")),
            "{human}"
        );
        assert!(
            human.contains(&format!("parked symbols unavailable: {waiting}")),
            "{human}"
        );
        let json = serde_json::to_value(&report).unwrap();
        let reasons = json["threads"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|thread| {
                Some((
                    thread["id"].as_str()?.to_string(),
                    thread["symbols_unavailable"].as_str()?.to_string(),
                ))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                ("cool".to_string(), past_cutoff.to_string()),
                ("parked".to_string(), waiting.to_string()),
            ]
        );
    }

    #[test]
    fn hot_leaf_is_reported_as_an_exact_stack() {
        let node = |parent: Option<usize>, samples: u64, frame: &str| SampleNode {
            parent,
            samples,
            frame: frame.to_string(),
            offset: None,
        };
        let thread = SampleThread {
            id: "1".to_string(),
            name: "worker".to_string(),
            declared_samples: 10,
            nodes: vec![
                node(None, 10, "root"),
                node(Some(0), 8, "hot"),
                node(Some(0), 2, "cold"),
            ],
            symbols_unavailable: None,
        };
        let report = thread_report(&thread);
        assert_eq!(report.heaviest_paths.len(), 1);
        assert_eq!(report.heaviest_paths[0].samples, 8);
        assert_eq!(report.heaviest_paths[0].frames, vec!["root", "hot"]);
        assert!(!report.heaviest_paths[0].truncated_at_fanout);
    }

    #[test]
    fn unresolved_image_frames_stay_raw_and_the_thread_says_why() {
        let raw = include_str!("../../tests/fixtures/ck-aft-health-rollup-sample.txt");
        let image = Path::new("/Users/someone/bin/ck-aft");
        let resolved = "aft::subc::health::build_health_diagnostic_rollup_from_actors (in ck-aft) (health.rs:10)";

        // Every image offset resolves: no gap is reported.
        let mut threads = parse_macos_sample(raw);
        let symbols = image_offsets(&threads, image)
            .into_iter()
            .map(|offset| (offset.clone(), format!("symbol_{offset} (in ck-aft)")))
            .collect::<HashMap<_, _>>();
        apply_symbols(&mut threads, &symbols, image);
        assert_eq!(threads[0].symbols_unavailable, None);
        assert_eq!(partial_symbols_warning(&threads), None);
        assert!(threads[0]
            .nodes
            .iter()
            .all(|node| !node.frame.starts_with("???")));

        // `atos` echoes an unknown offset as a hex address, and an offset it
        // printed nothing for is missing from the map. Both stay raw.
        let mut threads = parse_macos_sample(raw);
        let mut symbols = HashMap::new();
        symbols.insert("0x955a58".to_string(), resolved.to_string());
        symbols.insert(
            "0x1355eb4".to_string(),
            "0x0000000101355eb4 (in ck-aft)".to_string(),
        );
        apply_symbols(&mut threads, &symbols, image);
        let reason = threads[0].symbols_unavailable.clone().unwrap();
        let warning = partial_symbols_warning(&threads).unwrap();
        assert!(warning.contains("on 1 of 1 thread(s)"), "{warning}");
        assert!(
            reason.contains("did not resolve in the debug symbols"),
            "{reason}"
        );
        assert!(reason.contains("ck-aft"), "{reason}");

        // Checked on the parsed frames rather than on the ranked paths, so this
        // test covers symbol handling independently of path ranking.
        let frame_at = |offset: &str| {
            threads[0]
                .nodes
                .iter()
                .find(|node| node.offset.as_deref() == Some(offset))
                .map(|node| node.frame.clone())
                .unwrap()
        };
        assert_eq!(frame_at("0x955a58"), resolved);
        assert!(frame_at("0x1355eb4").contains("load address 0x1043fc000 + 0x1355eb4"));
        assert!(frame_at("0x1f096b8").contains("load address 0x1043fc000 + 0x1f096b8"));
        let report = thread_report(&threads[0]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["symbols_unavailable"], reason.as_str());
    }

    #[test]
    fn symbolization_failure_keeps_raw_stacks_and_names_the_reason() {
        let raw = include_str!("../../tests/fixtures/ck-aft-health-rollup-sample.txt");
        let mut threads = parse_macos_sample(raw);
        threads.push(SampleThread {
            id: "2".to_string(),
            name: "system-only".to_string(),
            declared_samples: 4,
            nodes: vec![SampleNode {
                parent: None,
                samples: 4,
                frame: "kevent  (in libsystem_kernel.dylib) + 8  [0x1832d6fcc]".to_string(),
                offset: None,
            }],
            symbols_unavailable: None,
        });
        let reason = "no cached debug symbols for loaded UUID ABC";
        mark_symbols_unavailable(&mut threads, Path::new("/Users/someone/bin/ck-aft"), reason);
        assert_eq!(threads[1].symbols_unavailable, None);

        let report = thread_report(&threads[0]);
        assert_eq!(report.symbols_unavailable.as_deref(), Some(reason));
        assert!(threads[0]
            .nodes
            .iter()
            .any(|node| node.frame.starts_with("???  (in ck-aft)  load address")));
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
            symbols_unavailable: None,
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
            image_on_disk_differs: false,
            image_on_disk_status: "readable".to_string(),
            image_on_disk_debug_id: None,
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
            truncated_at_fanout: false,
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
            symbols_unavailable: None,
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
    fn mismatched_dsym_planted_under_requested_key_is_refused_with_both_uuids() {
        let storage = tempfile::tempdir().expect("storage");
        let requested = "2CD06659-A5DD-3A77-8CB8-6C3469841BF4";
        let found = "E570EF4A-1111-2222-3333-444444444444";
        let planted = debug_cache_dir_from(storage.path(), requested).join("aft.dSYM");
        fs::create_dir_all(&planted).expect("mismatched dSYM fixture");

        let error = validate_debug_artifact_uuid(requested, found, &planted)
            .expect_err("a directory key cannot authenticate the dSYM inside it");
        let message = error.to_string();
        assert!(message.contains("2CD06659A5DD3A778CB86C3469841BF4"));
        assert!(message.contains("E570EF4A111122223333444444444444"));
        assert!(message.contains(&planted.display().to_string()));
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
        // dsymutil writes a bundle outside any TempDir; remove it however the
        // test ends.
        struct RemoveBundleOnDrop(std::path::PathBuf);
        impl Drop for RemoveBundleOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _dsym_cleanup = RemoveBundleOnDrop(dsym.clone());
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
