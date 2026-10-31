//! `aft backups purge`: remove undo backups early, by recorded path, by
//! session, or both.
//!
//! A running AFT daemon keeps backup stacks cached in memory, so when one owns
//! the storage the purge is sent to it as the `backups.purge` management
//! operation and it removes cache, files and database rows together. With no
//! owner the command runs the same purge code itself. See
//! `aft::backup::purge` for the locking that keeps both paths consistent.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use aft::backup::purge::{
    backup_key_for_path, purge_offline, PurgeReport, PurgeRequest, BACKUPS_PURGE_OPERATION,
};
use subc_client_rs::{CallOptions, CloseRouteOptions, ConsumerOptions};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};

/// A purge of a very large tree removes thousands of stacks; allow it to run
/// well past an ordinary request deadline before giving up on the reply.
const DAEMON_PURGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug)]
pub struct BackupsError {
    message: String,
    exit_code: i32,
}

impl BackupsError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    fn failure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for BackupsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackupsError {}

#[derive(Debug, Default, PartialEq, Eq)]
struct PurgeArgs {
    path: Option<PathBuf>,
    session: Option<String>,
    harness: Option<String>,
    yes: bool,
    json: bool,
    help: bool,
}

/// What the process that owns the backup store answered.
pub(crate) enum OwnerReply {
    /// No reachable process owns this storage, so the caller purges it.
    NotOwned(String),
    /// The owner ran the purge.
    Report(Box<PurgeReport>),
    /// An owner exists but the purge through it failed. The caller must not
    /// purge behind its back.
    Failed(String),
}

/// A process that may hold the backup store for the requested storage.
pub(crate) trait StoreOwner {
    fn purge(&self, request: &PurgeRequest) -> OwnerReply;
}

/// Execute `aft backups <subcommand>`.
pub fn run(args: Vec<OsString>) -> Result<(), BackupsError> {
    let current_dir = std::env::current_dir().map_err(|error| {
        BackupsError::failure(format!("could not read current directory: {error}"))
    })?;
    run_with(
        args,
        aft::bash_background::storage_dir(None),
        current_dir,
        &DaemonOwner,
        &mut io::stdout().lock(),
    )
}

fn run_with(
    args: Vec<OsString>,
    storage_dir: PathBuf,
    current_dir: PathBuf,
    owner: &dyn StoreOwner,
    output: &mut impl Write,
) -> Result<(), BackupsError> {
    let args = parse_args(args)?;
    if args.help {
        write_out(output, USAGE)?;
        return Ok(());
    }
    if !storage_dir.is_absolute() {
        return Err(BackupsError::usage(format!(
            "AFT_STORAGE_DIR must be absolute when set: {}",
            storage_dir.display()
        )));
    }

    let path = args.path.map(|path| {
        let absolute = if path.is_absolute() {
            path
        } else {
            current_dir.join(path)
        };
        backup_key_for_path(&absolute)
    });
    let request = PurgeRequest {
        storage_dir,
        path,
        session: args.session,
        harness: args.harness,
        dry_run: !args.yes,
    };
    request.validate().map_err(BackupsError::usage)?;

    let (ran_by, report) = purge_through_owner(&request, owner)?;
    if args.json {
        let body = serde_json::json!({ "ran_by": ran_by, "report": report });
        write_out(output, &format!("{body:#}\n"))?;
    } else {
        write_out(output, &render_report(&ran_by, &request, &report))?;
    }

    if report.is_complete() {
        Ok(())
    } else {
        Err(BackupsError::failure(format!(
            "{} backup stack(s) were not fully removed; they are listed above",
            report.failures.len()
        )))
    }
}

/// Send the purge to the owning process, or run it here when nothing owns the
/// storage. Running it here while an owner is alive would leave that owner's
/// cache describing stacks that are gone.
fn purge_through_owner(
    request: &PurgeRequest,
    owner: &dyn StoreOwner,
) -> Result<(String, PurgeReport), BackupsError> {
    match owner.purge(request) {
        OwnerReply::Report(report) => Ok(("daemon".to_string(), *report)),
        OwnerReply::NotOwned(reason) => purge_offline(request)
            .map(|report| (format!("this process ({reason})"), report))
            .map_err(BackupsError::failure),
        OwnerReply::Failed(message) => Err(BackupsError::failure(format!(
            "the running AFT daemon owns these backups, but the purge through it failed: \
             {message}. Nothing was purged by this command directly; stop the daemon to \
             purge offline."
        ))),
    }
}

const USAGE: &str = "\
Usage: aft backups purge [--path <file-or-dir>] [--session <id>] [--harness <name>] [--yes] [--json]

Remove undo backups early. At least one of --path or --session is required.

  --path <p>      entries recorded for this file, or for anything under this directory
  --session <id>  entries of one session (with --path: only that session's entries under it)
  --harness <n>   only this harness's storage namespace (opencode, pi, runner, mcp--..., fed--...)
  --yes           remove; without it the command only reports what would be removed
  --json          print the report as JSON

Runs through the AFT daemon when one owns the storage, so its in-memory undo
state is cleared together with the files and database rows.
";

fn parse_args(args: Vec<OsString>) -> Result<PurgeArgs, BackupsError> {
    let mut args = args.into_iter();
    let subcommand = args.next().map(|arg| arg.to_string_lossy().into_owned());
    match subcommand.as_deref() {
        Some("purge") => {}
        Some("--help" | "-h") | None => {
            return Ok(PurgeArgs {
                help: true,
                ..PurgeArgs::default()
            })
        }
        Some(other) => {
            return Err(BackupsError::usage(format!(
                "unknown backups subcommand {other:?}; expected `purge`"
            )))
        }
    }

    let mut parsed = PurgeArgs::default();
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|arg| BackupsError::usage(format!("argument is not UTF-8: {arg:?}")))?;
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (arg, None),
        };
        let mut value = |name: &str| -> Result<OsString, BackupsError> {
            match inline.clone() {
                Some(value) => Ok(OsString::from(value)),
                None => args
                    .next()
                    .ok_or_else(|| BackupsError::usage(format!("{name} needs a value"))),
            }
        };
        match flag.as_str() {
            "--path" => set_once(&mut parsed.path, PathBuf::from(value("--path")?), "--path")?,
            "--session" => set_once(
                &mut parsed.session,
                utf8(value("--session")?, "--session")?,
                "--session",
            )?,
            "--harness" => set_once(
                &mut parsed.harness,
                utf8(value("--harness")?, "--harness")?,
                "--harness",
            )?,
            "--yes" | "-y" => parsed.yes = true,
            "--json" => parsed.json = true,
            "--help" | "-h" => parsed.help = true,
            other => {
                return Err(BackupsError::usage(format!(
                    "unknown argument {other:?}\n\n{USAGE}"
                )))
            }
        }
    }
    Ok(parsed)
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), BackupsError> {
    if slot.is_some() {
        return Err(BackupsError::usage(format!("{name} given more than once")));
    }
    *slot = Some(value);
    Ok(())
}

fn utf8(value: OsString, name: &str) -> Result<String, BackupsError> {
    value
        .into_string()
        .map_err(|_| BackupsError::usage(format!("{name} must be UTF-8")))
}

fn write_out(output: &mut impl Write, text: &str) -> Result<(), BackupsError> {
    output
        .write_all(text.as_bytes())
        .map_err(|error| BackupsError::failure(format!("could not write output: {error}")))
}

fn render_report(ran_by: &str, request: &PurgeRequest, report: &PurgeReport) -> String {
    let mut text = String::new();
    let heading = if report.dry_run {
        "aft backups purge: dry run, nothing removed (rerun with --yes to remove)"
    } else {
        "aft backups purge"
    };
    text.push_str(heading);
    text.push('\n');
    text.push_str(&format!("ran by: {ran_by}\n"));
    text.push_str(&format!("storage: {}\n", report.storage_dir));
    text.push_str(&format!(
        "filter: path={} session={} harness={}\n",
        request
            .path
            .as_ref()
            .map_or("any".to_string(), |path| path.display().to_string()),
        request.session.as_deref().unwrap_or("any"),
        request.harness.as_deref().unwrap_or("any"),
    ));
    let examined = &report.examined;
    text.push_str(&format!(
        "examined: {} namespace(s) [{}], {} session dir(s), {} disk stack(s), {} database stack(s), {} cached stack(s), {} skipped\n",
        examined.namespaces.len(),
        examined.namespaces.join(", "),
        examined.session_dirs,
        examined.disk_stacks,
        examined.db_stacks,
        examined.memory_stacks,
        examined.skipped,
    ));
    for skipped in &examined.skipped_samples {
        text.push_str(&format!("  skipped {skipped}\n"));
    }
    text.push_str(&format!("matched: {}\n", render_totals(&report.matched)));
    if !report.dry_run {
        text.push_str(&format!("removed: {}\n", render_totals(&report.removed)));
    }
    if !report.samples.is_empty() {
        text.push_str("sample paths:\n");
        for sample in &report.samples {
            text.push_str(&format!("  {sample}\n"));
        }
    }
    if !report.failures.is_empty() {
        text.push_str(&format!("not removed ({}):\n", report.failures.len()));
        for failure in &report.failures {
            let state = if failure.hidden {
                "hidden from undo, files left"
            } else {
                "still present"
            };
            text.push_str(&format!(
                "  [{}] session {} {} ({state}): {}\n",
                failure.harness, failure.session, failure.path, failure.error
            ));
        }
    }
    text
}

fn render_totals(totals: &aft::backup::purge::PurgeTotals) -> String {
    format!(
        "{} stack(s), {} entr{}, {} file(s), {} byte(s), {} database row(s), {} session(s){}",
        totals.stacks,
        totals.entries,
        if totals.entries == 1 { "y" } else { "ies" },
        totals.files,
        totals.bytes,
        totals.db_rows,
        totals.sessions.len(),
        if totals.sessions.is_empty() {
            String::new()
        } else {
            format!(" [{}]", totals.sessions.join(", "))
        },
    )
}

/// The AFT module attached to the local subc daemon, reached through its
/// published connection file.
struct DaemonOwner;

impl StoreOwner for DaemonOwner {
    fn purge(&self, request: &PurgeRequest) -> OwnerReply {
        let Some(connection_file) = aft::gh_shim::configured_connection_file() else {
            return OwnerReply::NotOwned("no AFT daemon connection file".to_string());
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                return OwnerReply::Failed(format!("could not start daemon client: {error}"))
            }
        };
        runtime.block_on(purge_via_daemon(connection_file, request))
    }
}

async fn purge_via_daemon(connection_file: PathBuf, request: &PurgeRequest) -> OwnerReply {
    let consumer = match aft::fleet_status::connect_subc_consumer(
        &connection_file,
        ConsumerOptions::default(),
    )
    .await
    {
        Ok(consumer) => consumer,
        // A connection file without a listening daemon is left behind by a
        // daemon that exited; nothing holds the store.
        Err(error) => return OwnerReply::NotOwned(format!("no AFT daemon reachable: {error}")),
    };
    let catalog = match consumer.catalog_list().await {
        Ok(catalog) => catalog,
        Err(error) => return OwnerReply::Failed(format!("daemon catalog failed: {error}")),
    };
    let Some(aft_entry) = catalog
        .modules
        .iter()
        .find(|entry| entry.module_id == "aft")
    else {
        return OwnerReply::NotOwned("the daemon hosts no AFT module".to_string());
    };
    let advertises_purge = aft_entry.roles.iter().any(|role| {
        matches!(
            role,
            ProviderRole::ManagementSurface { operations, .. }
                if operations.iter().any(|operation| operation.name == BACKUPS_PURGE_OPERATION)
        )
    });
    if !advertises_purge {
        return OwnerReply::Failed(format!(
            "the running AFT daemon does not offer {BACKUPS_PURGE_OPERATION}; restart it on this AFT version"
        ));
    }

    let project_root = request.storage_dir.clone();
    let route = match consumer
        .open_route(
            RouteTarget::ManagementSurface {
                module_id: "aft".to_string(),
            },
            BindIdentity::new(
                "aft-backups-purge",
                format!("aft-backups-purge-{}", std::process::id()),
            ),
            CallOptions::default(),
        )
        .await
    {
        Ok(route) => route,
        Err(error) => return OwnerReply::Failed(format!("route failed: {error}")),
    };
    let body = match serde_json::to_vec(&serde_json::json!({
        "op": BACKUPS_PURGE_OPERATION,
        "params": request,
    })) {
        Ok(body) => body,
        Err(error) => return OwnerReply::Failed(format!("could not encode request: {error}")),
    };
    let options = CallOptions {
        timeout: DAEMON_PURGE_TIMEOUT,
        ..CallOptions::default()
    };
    let response = consumer.request(&route, body, options).await;
    let _ = consumer
        .close_handle(&route, CloseRouteOptions::default())
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => return OwnerReply::Failed(format!("request failed: {error}")),
    };
    owner_reply_from_envelope(&response)
}

fn owner_reply_from_envelope(response: &[u8]) -> OwnerReply {
    let envelope: serde_json::Value = match serde_json::from_slice(response) {
        Ok(envelope) => envelope,
        Err(error) => return OwnerReply::Failed(format!("invalid response: {error}")),
    };
    let data = envelope.get("data").cloned().unwrap_or_default();
    if envelope.get("status").and_then(serde_json::Value::as_str) == Some("ok") {
        return match serde_json::from_value::<PurgeReport>(data) {
            Ok(report) => OwnerReply::Report(Box::new(report)),
            Err(error) => OwnerReply::Failed(format!("unreadable purge report: {error}")),
        };
    }
    let code = data
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let message = data
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no message");
    if code == "storage_not_owned" {
        OwnerReply::NotOwned(format!("the daemon does not use this storage: {message}"))
    } else {
        OwnerReply::Failed(format!("{code}: {message}"))
    }
}

#[cfg(test)]
mod tests;
