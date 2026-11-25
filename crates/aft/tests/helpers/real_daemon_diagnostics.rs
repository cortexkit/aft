//! Credential-free event records for the isolated real-daemon CI cases.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

fn directory() -> Option<PathBuf> {
    std::env::var_os("AFT_REAL_DAEMON_CASE_DIR").map(PathBuf::from)
}

pub fn enabled() -> bool {
    directory().is_some()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn phase(name: &'static str) {
    if let Some(directory) = directory() {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("timeline"))
        {
            let _ = writeln!(file, "{} {name}", now_ms());
        }
    }
}

pub fn install_daemon_trace() {
    let Some(directory) = directory() else { return };
    let Ok(file) = File::create(directory.join("daemon.log")) else {
        return;
    };
    // Each CI case is a separate process. A global subscriber also sees the
    // embedded daemon's spawned tasks, which a thread-local subscriber misses.
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(DaemonTrace(Mutex::new((file, 0)))),
    );
}

struct DaemonTrace(Mutex<(File, usize)>);

#[derive(Default)]
struct Fields {
    message: Option<&'static str>,
    module: Option<&'static str>,
    code: Option<&'static str>,
    reason: Option<&'static str>,
}

impl Fields {
    fn record_choice(&mut self, field: &Field, value: &str) {
        // Never persist formatted fields, request or error text. Even an
        // isolated test daemon handles a live nonce and connection secret.
        let (slot, choices): (_, &[_]) = match field.name() {
            "message" => (
                &mut self.message,
                &[
                    "configured module registered",
                    "module registered",
                    "route.open accepted",
                    "route.open refused",
                    "route.open refusing module: not serving",
                    "route.open accepted again after module outage",
                    "supervised module exited cleanly",
                    "supervised module exited abnormally (crash)",
                ],
            ),
            "module_id" => (
                &mut self.module,
                &[
                    "aft",
                    "aft-test-scope-owner",
                    "broca",
                    "thalamus-harness",
                    "claustrum",
                ],
            ),
            "code" | "module_code" => (
                &mut self.code,
                &[
                    "module_warming",
                    "target_unavailable",
                    "module_timeout",
                    "module_rejected",
                    "unsupported_role_version",
                    "unknown_module",
                    "scope_ended",
                    "scope_changed",
                ],
            ),
            "reason" => (
                &mut self.reason,
                &[
                    "supervised_not_registered",
                    "declared_not_ready",
                    "waiting_for_previous_pid",
                    "module_rejected",
                    "protocol_none",
                ],
            ),
            _ => return,
        };
        *slot = choices.iter().copied().find(|&choice| value == choice);
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_choice(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if matches!(
            field.name(),
            "message" | "module_id" | "code" | "module_code" | "reason"
        ) {
            self.record_choice(field, format!("{value:?}").trim_matches('"'));
        }
    }
}

impl<S: Subscriber> Layer<S> for DaemonTrace {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let meta = event.metadata();
        if !meta
            .module_path()
            .is_some_and(|path| path.starts_with("subc_daemon::"))
        {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.message == Some("route.open accepted") && fields.module == Some("aft") {
            phase("daemon_route_accepted_aft");
        }
        if fields.message.is_none() && !matches!(*meta.level(), Level::WARN | Level::ERROR) {
            return;
        }
        let Ok(mut sink) = self.0.lock() else { return };
        if sink.1 >= 1024 {
            return;
        }
        sink.1 += 1;
        let source = meta
            .file()
            .and_then(|file| Path::new(file).file_name())
            .and_then(|file| file.to_str())
            .unwrap_or("unknown");
        let _ = writeln!(
            sink.0,
            "{} {} {}:{} module={} code={} reason={} {}",
            now_ms(),
            meta.level(),
            source,
            meta.line().unwrap_or(0),
            fields.module.unwrap_or("-"),
            fields.code.unwrap_or("-"),
            fields.reason.unwrap_or("-"),
            fields.message.unwrap_or("details omitted")
        );
    }
}
