//! Live reload of AFT config file edits.
//!
//! A bound root normally reads `~/.config/cortexkit/aft.jsonc` and
//! `<root>/.cortexkit/aft.jsonc` only when a session connects (configure).
//! This module applies an edit to either file while the root stays bound, but
//! only for the keys that are safe to change under running sessions: the ones
//! read from a fresh config snapshot on every call, plus a few that need one
//! existing setter pushed. Every other changed key is logged as deferred and
//! is applied by the next configure, exactly as before.
//!
//! The reload never runs `handle_configure`. It re-resolves both files with the
//! same resolver and trust boundary, copies only the live keys onto the
//! published snapshot and publishes that with a compare-and-swap. It runs on a
//! maintenance lane that holds the root's read gate, so it never waits for
//! in-flight requests; each request keeps the snapshot it pinned when it was
//! admitted (`AppContext::pin_config`).
//!
//! The design is in `docs/design/config-live-reload.md`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::config_resolve::ConfigTier;
use crate::context::AppContext;

/// How long a burst of file events must be quiet before the reload runs.
/// Editors save by writing a temporary sibling and renaming it over the file,
/// which arrives as several events.
pub const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

/// File name both config tiers use.
const CONFIG_FILE_NAME: &str = "aft.jsonc";

fn process_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    u64::try_from(process_epoch().elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A pending "re-read the config files" request for one root, with a trailing
/// debounce: every new request pushes the due time out again.
#[derive(Debug, Default)]
pub struct ConfigReloadSignal {
    /// Milliseconds since the process epoch at which the reload is due; zero
    /// when no reload is pending.
    due_at_ms: AtomicU64,
}

impl ConfigReloadSignal {
    /// Ask for a reload once the debounce window has passed quietly.
    pub fn request(&self) {
        self.request_after(CONFIG_RELOAD_DEBOUNCE);
    }

    fn request_after(&self, delay: Duration) {
        let due = now_ms()
            .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
            .max(1);
        self.due_at_ms.store(due, Ordering::Release);
    }

    /// Ask for a reload that is due at once (used when a root reattaches after
    /// idle eviction: the content check makes an unneeded reload a no-op).
    pub fn request_now(&self) {
        self.request_after(Duration::ZERO);
    }

    pub fn is_pending(&self) -> bool {
        self.due_at_ms.load(Ordering::Acquire) != 0
    }

    pub fn is_due(&self) -> bool {
        let due = self.due_at_ms.load(Ordering::Acquire);
        due != 0 && due <= now_ms()
    }

    /// Clear a due request and report whether there was one. A request that
    /// arrives while the reload runs sets a new due time and is not lost.
    pub fn take_if_due(&self) -> bool {
        let due = self.due_at_ms.load(Ordering::Acquire);
        if due == 0 || due > now_ms() {
            return false;
        }
        self.due_at_ms
            .compare_exchange(due, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// What one tier's text was when it was last applied, and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSource {
    /// `user` or `project`.
    pub tier: &'static str,
    /// The config file for this tier, when known.
    pub path: Option<PathBuf>,
    /// The text applied last, or `None` when the tier was absent.
    pub text: Option<String>,
    /// Whether `text` was read from `path`. A tier relayed on the wire by an
    /// older plugin whose file does not exist is kept as it is.
    pub from_file: bool,
}

/// What the last configure applied for a root, so a reload can tell whether
/// the files changed and which changed keys are still waiting for a connect.
#[derive(Debug, Clone)]
pub struct ConfigSources {
    pub user: TierSource,
    pub project: TierSource,
    /// The resolver output of the last configure, before configure's own
    /// adjustments. Deferred keys are reported relative to it.
    pub connected: Config,
    /// Whether this root's context watches the user file itself. False in the
    /// daemon, where one process-level watch serves every root.
    pub owns_user_watch: bool,
}

impl ConfigSources {
    /// Record what configure resolved from. `tiers` is what configure passed to
    /// the resolver: a tier whose `source` names the tier's file was read from
    /// that file (the daemon's bind reads the files and relays them this way).
    pub fn from_configure(
        user_config_path: Option<PathBuf>,
        project_root: &Path,
        tiers: &[ConfigTier],
        connected: &Config,
    ) -> Self {
        let owns_user_watch = user_config_path.is_some() && process_user_config_path().is_none();
        let user_path = user_config_path.or_else(process_user_config_path);
        let project_path =
            crate::subc_config::project_config_read_path(project_root, connected.harness.as_ref());
        let source_for = |tier: &'static str, path: Option<PathBuf>| {
            let used = tiers.iter().find(|candidate| candidate.tier == tier);
            let from_file = match (used, path.as_ref()) {
                (Some(used), Some(path)) => Path::new(&used.source) == path.as_path(),
                (None, _) => true,
                (Some(_), None) => false,
            };
            TierSource {
                tier,
                path,
                text: used.map(|used| used.doc.clone()),
                from_file,
            }
        };
        Self {
            user: source_for("user", user_path),
            project: source_for("project", Some(project_path)),
            connected: connected.clone(),
            owns_user_watch,
        }
    }
}

/// `<root>/.cortexkit/aft.jsonc`.
pub fn project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".cortexkit").join(CONFIG_FILE_NAME)
}

/// Whether `path` names a shared or legacy project config, or an editor's
/// temporary sibling. `root` is the configured canonical directory; the
/// event's containing root is canonicalized if needed, while filenames stay literal.
pub fn is_project_config_event_path(root: &Path, path: &Path) -> bool {
    if !is_config_file_name(path) {
        return false;
    }
    let Some(dir) = path.parent() else {
        return false;
    };
    if !matches!(
        dir.file_name().and_then(|name| name.to_str()),
        Some(".cortexkit" | ".opencode" | ".pi")
    ) {
        return false;
    }
    let Some(dir_root) = dir.parent() else {
        return false;
    };
    // Raw watcher events may spell the root differently from the configured
    // canonical root (macOS reports `/private/var` for `/var`); resolve only
    // the root directory, never the config file name itself.
    dir_root == root || std::fs::canonicalize(dir_root).is_ok_and(|resolved| resolved == root)
}

fn is_config_file_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("aft.json") || name.starts_with(".aft.json"))
}

static PROCESS_USER_CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// The daemon watches the user config file once for the whole process and
/// records its path here, so root contexts do not each start a watch.
pub fn set_process_user_config_path(path: PathBuf) {
    let _ = PROCESS_USER_CONFIG_PATH.set(path);
}

fn process_user_config_path() -> Option<PathBuf> {
    PROCESS_USER_CONFIG_PATH.get().cloned()
}

thread_local! {
    static STAGED_SOURCES: RefCell<Option<ConfigSources>> = const { RefCell::new(None) };
}

/// Called by configure once its candidate resolved. The sources become the
/// root's record only if configure then succeeds ([`finish_configure`]).
pub fn stage_configure_sources(sources: ConfigSources) {
    STAGED_SOURCES.with(|staged| *staged.borrow_mut() = Some(sources));
}

/// Called when configure returns. A successful configure applied everything
/// in the files, so its sources replace the record and any pending reload for
/// that content is moot. A failed one leaves the record as it was.
pub fn finish_configure(ctx: &AppContext, success: bool) {
    let Some(sources) = STAGED_SOURCES.with(|staged| staged.borrow_mut().take()) else {
        return;
    };
    if !success {
        return;
    }
    {
        let _reload = ctx.config_live().reload_lock.lock();
        *ctx.config_live().sources.lock() = Some(sources);
        // A connect resolves the files afresh; nothing it applied is held.
        ctx.config_live()
            .project_hold_active
            .store(false, Ordering::Release);
    }
    sync_config_watches(ctx);
}

/// Per-root live reload state, owned by the root's `AppContext`.
#[derive(Default)]
pub struct ConfigLiveState {
    signal: Arc<ConfigReloadSignal>,
    sources: parking_lot::Mutex<Option<ConfigSources>>,
    watches: parking_lot::Mutex<ContextWatches>,
    /// Whether the running project watcher covers `<root>/.cortexkit/`.
    project_watcher_sees_config: AtomicBool,
    /// The last reload error logged, so a file that stays broken is reported once.
    last_error: parking_lot::Mutex<Option<String>>,
    /// The last reload report, for tests and status.
    last_outcome: parking_lot::Mutex<Option<ReloadOutcome>>,
    /// Whether a project loosening is being held. While it is, the published
    /// security values are part of the floor, because they may include
    /// hardening from project texts that were never recorded (a later project
    /// edit made while an earlier one was held). Cleared by a connect.
    project_hold_active: AtomicBool,
    /// Serializes a reload's read-resolve-publish-record sequence with
    /// configure's recording of what it applied, so a reload in flight cannot
    /// set `project_hold_active` after a configure cleared it.
    reload_lock: parking_lot::Mutex<()>,
}

#[derive(Default)]
struct ContextWatches {
    user: Option<ConfigFileWatch>,
    project_fallback: Option<ConfigFileWatch>,
}

impl ConfigLiveState {
    pub fn signal(&self) -> Arc<ConfigReloadSignal> {
        Arc::clone(&self.signal)
    }

    /// Whether a reload is due now (debounce elapsed).
    pub fn reload_due(&self) -> bool {
        self.signal.is_due()
    }

    pub fn sources(&self) -> Option<ConfigSources> {
        self.sources.lock().clone()
    }

    pub fn last_outcome(&self) -> Option<ReloadOutcome> {
        self.last_outcome.lock().clone()
    }

    pub fn set_project_watcher_sees_config(&self, sees: bool) {
        self.project_watcher_sees_config
            .store(sees, Ordering::Release);
    }

    pub fn has_project_fallback_watch(&self) -> bool {
        self.watches.lock().project_fallback.is_some()
    }

    pub fn has_user_watch(&self) -> bool {
        self.watches.lock().user.is_some()
    }
}

/// Start or stop this root's own file watches to match its state: a user-file
/// watch when the context owns one (standalone), and a project-file watch when
/// the project watcher does not cover the selected file's directory. Legacy
/// files always retain a direct watch because the coverage flag certifies only
/// the extra `.cortexkit` watch, not legacy directories or symlink targets.
pub fn sync_config_watches(ctx: &AppContext) {
    if config_watches_disabled() {
        return;
    }
    let state = ctx.config_live();
    let Some(sources) = state.sources() else {
        return;
    };
    let shared_project_file = sources.project.path.as_ref().is_some_and(|path| {
        path.file_name() == Some(std::ffi::OsStr::new(CONFIG_FILE_NAME))
            && path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".cortexkit"))
    });
    let covered = shared_project_file
        && ctx.watcher_runtime_active()
        && state.project_watcher_sees_config.load(Ordering::Acquire);
    let mut watches = state.watches.lock();

    let wanted_user = sources
        .owns_user_watch
        .then_some(sources.user.path.clone())
        .flatten();
    if watches.user.as_ref().map(ConfigFileWatch::file) != wanted_user.as_deref() {
        watches.user = wanted_user.map(|path| {
            ConfigFileWatch::start_for_root(path, state.signal(), ctx.watcher_counters())
        });
    }

    let wanted_project = (!covered).then_some(sources.project.path.clone()).flatten();
    if watches.project_fallback.as_ref().map(ConfigFileWatch::file) != wanted_project.as_deref() {
        watches.project_fallback = wanted_project.map(|path| {
            ConfigFileWatch::start_for_root(path, state.signal(), ctx.watcher_counters())
        });
    }
}

/// Whether this process starts no config file watches of its own.
///
/// The integration suite spawns hundreds of `aft` processes with
/// `AFT_TEST_DISABLE_FILE_WATCHER=1` because OS watch registration can hang
/// under that load; the same switch covers these watches. Unit tests start
/// none either, unless a test turns them on, so the hundreds of configure unit
/// tests do not each leave a watch thread behind. The reload itself still
/// runs when asked.
fn config_watches_disabled() -> bool {
    if std::env::var("AFT_TEST_DISABLE_FILE_WATCHER").is_ok_and(|value| value == "1") {
        return true;
    }
    cfg!(test) && !CONFIG_WATCHES_ENABLED_FOR_TEST.load(Ordering::Acquire)
}

static CONFIG_WATCHES_ENABLED_FOR_TEST: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn enable_config_watches_for_test() {
    CONFIG_WATCHES_ENABLED_FOR_TEST.store(true, Ordering::Release);
}

/// What one reload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// No configure has run for this root yet.
    NotConfigured,
    /// The files hold the text that was applied last.
    Unchanged,
    /// A file could not be used; the last valid configuration stays.
    Kept { reason: String },
    /// The files were re-resolved. `applied` may be empty when only deferred
    /// keys changed.
    Reloaded {
        applied: Vec<&'static str>,
        deferred: Vec<&'static str>,
        dropped: Vec<String>,
        /// Security keys a project edit would have loosened; they keep their
        /// published value until the next connect.
        held: Vec<&'static str>,
    },
}

/// Run a pending reload for `ctx` if its debounce has elapsed.
pub fn drain_config_reload(ctx: &AppContext) -> Option<ReloadOutcome> {
    if !ctx.config_live().signal.take_if_due() {
        return None;
    }
    Some(reload_config_now(ctx))
}

enum TierRead {
    Use(TierSource),
    Error(String),
}

fn read_tier(recorded: &TierSource) -> TierRead {
    let Some(path) = recorded.path.as_ref() else {
        return TierRead::Use(recorded.clone());
    };
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => TierRead::Use(TierSource {
                text: Some(text),
                from_file: true,
                ..recorded.clone()
            }),
            Err(_) => TierRead::Error(format!("{} is not valid UTF-8", path.display())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if recorded.from_file && recorded.text.is_some() {
                // A deleted file is treated as unreadable, not as `{}`: a
                // security key must not loosen because a file vanished
                // mid-save or was deleted by mistake. The next connect still
                // resolves a missing file as `{}`.
                TierRead::Error(format!("{} was deleted", path.display()))
            } else {
                TierRead::Use(recorded.clone())
            }
        }
        Err(error) => TierRead::Error(format!("cannot read {}: {error}", path.display())),
    }
}

fn tier_for(source: &TierSource) -> Option<ConfigTier> {
    let text = source.text.as_ref()?;
    Some(ConfigTier {
        tier: source.tier.to_string(),
        source: source
            .path
            .as_ref()
            .filter(|_| source.from_file)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{} tier", source.tier)),
        doc: text.clone(),
    })
}

/// Re-read the config files for `ctx` and apply the live keys that changed.
pub fn reload_config_now(ctx: &AppContext) -> ReloadOutcome {
    let _reload = ctx.config_live().reload_lock.lock();
    let outcome = reload_config_inner(ctx);
    *ctx.config_live().last_outcome.lock() = Some(outcome.clone());
    outcome
}

fn reload_config_inner(ctx: &AppContext) -> ReloadOutcome {
    let state = ctx.config_live();
    let Some(sources) = state.sources() else {
        return ReloadOutcome::NotConfigured;
    };
    let root_label = ctx
        .config_unpinned()
        .project_root
        .as_ref()
        .map(|root| root.display().to_string())
        .unwrap_or_default();

    let (user, project) = match (read_tier(&sources.user), read_tier(&sources.project)) {
        (TierRead::Use(user), TierRead::Use(project)) => (user, project),
        (TierRead::Error(reason), _) | (_, TierRead::Error(reason)) => {
            return keep_last_good(ctx, &root_label, reason);
        }
    };
    // A held loosening is released once the project file is back to the text
    // the published values were resolved from: the edits that were held no
    // longer exist, so the published config is resolved from the files again.
    let was_holding = state.project_hold_active.load(Ordering::Acquire);
    let holding = was_holding && project.text != sources.project.text;
    if was_holding && !holding {
        state.project_hold_active.store(false, Ordering::Release);
    }
    if !was_holding && user.text == sources.user.text && project.text == sources.project.text {
        // The texts match, but a tier relayed on the wire may now come from
        // its file; record that so a later deletion of the file is noticed.
        if let Some(record) = state.sources.lock().as_mut() {
            record.user.from_file = user.from_file;
            record.project.from_file = project.from_file;
        }
        state.last_error.lock().take();
        return ReloadOutcome::Unchanged;
    }
    let project_changed = project.text != sources.project.text;
    // What the edit would resolve to if only the user file had changed. A
    // project edit may tighten security keys relative to it, never loosen.
    let user_only_tiers: Vec<ConfigTier> = [&user, &sources.project]
        .into_iter()
        .filter_map(tier_for)
        .collect();

    let tiers: Vec<ConfigTier> = [&user, &project].into_iter().filter_map(tier_for).collect();
    let harness = ctx.config_unpinned().harness.clone();
    for tier in &tiers {
        if let Some(reason) = crate::config_resolve::strict_tier_error(tier, harness.as_ref()) {
            return keep_last_good(ctx, &root_label, reason);
        }
    }

    // A configure that publishes between the read and the swap wins; the
    // resolve is then repeated once against its snapshot.
    let mut attempts = 0;
    let (applied, deferred, dropped, held) = loop {
        attempts += 1;
        let published = ctx.config_unpinned();
        let mut candidate = published.as_ref().clone();
        let diagnostics = crate::config_resolve::resolve_config_onto_with_diagnostics_for_harness(
            &tiers,
            published.harness.as_ref(),
            &mut candidate,
        );
        if !diagnostics.errors.is_empty() {
            return keep_last_good(
                ctx,
                &root_label,
                format!("configuration rejected: {}", diagnostics.errors.join(", ")),
            );
        }
        let dropped: Vec<String> = diagnostics
            .dropped
            .iter()
            .map(|drop| format!("{} ({})", drop.key, drop.reason))
            .collect();
        let held = if project_changed || holding {
            let mut floor = published.as_ref().clone();
            let user_only_diagnostics =
                crate::config_resolve::resolve_config_onto_with_diagnostics_for_harness(
                    &user_only_tiers,
                    published.harness.as_ref(),
                    &mut floor,
                );
            // The previous project text was accepted when it was applied; if
            // it no longer resolves, the published config is the floor.
            if !user_only_diagnostics.errors.is_empty() {
                floor = published.as_ref().clone();
            }
            // While a hold is active the published values carry hardening
            // from project texts that were never recorded; keep all of it.
            if holding {
                hold_project_loosening(&mut floor, &published);
            }
            hold_project_loosening(&mut candidate, &floor)
        } else {
            Vec::new()
        };
        let live = apply_live_config(&published, &candidate, &sources.connected);
        if live.applied.is_empty() {
            break (live.applied, live.deferred, dropped, held);
        }
        if ctx.publish_config_if_current(&published, live.next.clone()) {
            push_live_setters(ctx, &published, &live.next);
            break (live.applied, live.deferred, dropped, held);
        }
        if attempts >= 2 {
            // Two configures in a row published meanwhile; each of them read
            // the files itself, so there is nothing left to apply.
            return ReloadOutcome::Unchanged;
        }
    };

    {
        let mut record = state.sources.lock();
        if let Some(record) = record.as_mut() {
            record.user = user;
            // While a project loosening is held, keep the project text the
            // held values came from as the reference, so later reloads keep
            // holding it until a connect resolves the files afresh.
            if held.is_empty() {
                record.project = project;
            }
        }
        if !held.is_empty() {
            state.project_hold_active.store(true, Ordering::Release);
        }
    }
    state.last_error.lock().take();
    log_reload(&root_label, &applied, &deferred, &dropped, &held);
    ReloadOutcome::Reloaded {
        applied,
        deferred,
        dropped,
        held,
    }
}

/// Keep every security key of `candidate` at least as strict as in `floor`,
/// and name the keys that were held back.
///
/// `floor` is what the files would resolve to with the project file as it was
/// before this edit, so a project edit can add hardening but not remove it.
/// The next connect resolves the files afresh and applies a held loosening.
fn hold_project_loosening(candidate: &mut Config, floor: &Config) -> Vec<&'static str> {
    let mut held = Vec::new();
    if !candidate.lsp_idle_minutes.tightens(floor.lsp_idle_minutes) {
        candidate.lsp_idle_minutes = floor.lsp_idle_minutes;
        held.push("lsp.idle_minutes");
    }
    let next = &mut candidate.inspect.categories;
    let before = &floor.inspect.categories;
    macro_rules! keep_off {
        ($field:ident, $key:literal) => {
            if !before.$field && next.$field {
                next.$field = false;
                held.push($key);
            }
        };
    }
    keep_off!(diagnostics, "inspect.categories.diagnostics");
    keep_off!(todos, "inspect.categories.todos");
    keep_off!(dead_code, "inspect.categories.dead_code");
    keep_off!(unused_exports, "inspect.categories.unused_exports");
    keep_off!(duplicates, "inspect.categories.duplicates");
    keep_off!(cycles, "inspect.categories.cycles");
    keep_off!(complexity, "inspect.categories.complexity");
    if floor.restrict_to_project_root && !candidate.restrict_to_project_root {
        candidate.restrict_to_project_root = true;
        held.push("restrict_to_project_root");
    }
    if !floor.url_fetch_allow_private && candidate.url_fetch_allow_private {
        candidate.url_fetch_allow_private = false;
        held.push("url_fetch_allow_private");
    }
    if floor.sandbox.enabled && !candidate.sandbox.enabled {
        candidate.sandbox.enabled = true;
        held.push("sandbox.enabled");
    }
    if floor.bash.disclaim_privacy && !candidate.bash.disclaim_privacy {
        candidate.bash.disclaim_privacy = true;
        held.push("bash.disclaim_privacy");
    }
    let missing_denies: Vec<PathBuf> = floor
        .sandbox
        .read_deny
        .iter()
        .filter(|path| !candidate.sandbox.read_deny.contains(path))
        .cloned()
        .collect();
    if !missing_denies.is_empty() {
        candidate.sandbox.read_deny.extend(missing_denies);
        held.push("sandbox.read_deny");
    }
    let before = candidate.sandbox.write_allow.len();
    candidate
        .sandbox
        .write_allow
        .retain(|path| floor.sandbox.write_allow.contains(path));
    if candidate.sandbox.write_allow.len() != before {
        held.push("sandbox.write_allow");
    }
    held
}

fn keep_last_good(ctx: &AppContext, root_label: &str, reason: String) -> ReloadOutcome {
    let mut last = ctx.config_live().last_error.lock();
    if last.as_deref() != Some(reason.as_str()) {
        crate::slog_error!(
            "config reload root={}: {}; keeping the last valid configuration",
            root_label,
            reason
        );
        *last = Some(reason.clone());
    }
    ReloadOutcome::Kept { reason }
}

fn log_reload(root: &str, applied: &[&str], deferred: &[&str], dropped: &[String], held: &[&str]) {
    let mut line = reload_log_line(root, applied, deferred, dropped);
    if !held.is_empty() {
        line.push_str(&format!(
            " held=[{}] (not loosened while a project edit is held; the next connect applies them)",
            held.join(",")
        ));
    }
    if applied.is_empty() && deferred.is_empty() && held.is_empty() {
        crate::slog_debug!("{}", line);
    } else {
        crate::slog_info!("{}", line);
    }
}

/// The one log line a reload writes: the keys applied now, the changed keys
/// waiting for the next connect, and project values the trust boundary dropped.
pub fn reload_log_line(
    root: &str,
    applied: &[&str],
    deferred: &[&str],
    dropped: &[String],
) -> String {
    let mut line = format!("config reload root={root}");
    if !applied.is_empty() {
        line.push_str(&format!(" applied=[{}]", applied.join(",")));
    }
    if !deferred.is_empty() {
        line.push_str(&format!(
            " deferred=[{}] (deferred keys apply on next connect/restart)",
            deferred.join(",")
        ));
    }
    if !dropped.is_empty() {
        line.push_str(&format!(
            " dropped=[{}] (project may only tighten)",
            dropped.join(",")
        ));
    }
    if applied.iter().any(|key| key.starts_with("sandbox.")) {
        line.push_str("; running background tasks keep the sandbox they started with");
    }
    line
}

/// Call the setters that copy a changed live key into a component that keeps
/// its own copy.
fn push_live_setters(ctx: &AppContext, before: &Config, after: &Config) {
    if before.formatter != after.formatter || before.checker != after.checker {
        crate::format::clear_tool_cache_for_root(after.project_root.as_deref());
    }
    if before.backup.max_file_size != after.backup.max_file_size {
        // `enabled` and `max_depth` are not live; keep what configure set.
        let mut backup = ctx.backup().lock();
        backup.set_policy(crate::backup::BackupPolicy {
            enabled: after.backup.enabled.unwrap_or(true),
            max_depth: after
                .backup
                .max_depth
                .unwrap_or(crate::backup::DEFAULT_MAX_UNDO_DEPTH),
            max_file_size: Some(
                after
                    .backup
                    .max_file_size
                    .unwrap_or(crate::backup::DEFAULT_MAX_BACKUP_FILE_SIZE),
            ),
        });
    }
    if before.bash_long_running_reminder_enabled != after.bash_long_running_reminder_enabled
        || before.bash_long_running_reminder_interval_ms
            != after.bash_long_running_reminder_interval_ms
    {
        ctx.bash_background().configure_long_running_reminders(
            after.bash_long_running_reminder_enabled,
            after.bash_long_running_reminder_interval_ms,
        );
    }
    if before.inspect.enabled != after.inspect.enabled
        || before.inspect.categories != after.inspect.categories
    {
        ctx.reset_tier2_refresh_scheduler();
    } else if before.inspect != after.inspect {
        // Inspect settings shape Tier-2 results (for example duplicates'
        // expected mirrors), so a failed category may now succeed.
        ctx.inspect_manager().clear_tier2_retry_pauses();
    }
    if before.git.co_author == "off" && after.git.co_author != "off" {
        let storage_root = crate::bash_background::storage_dir(after.storage_dir.as_deref());
        if let Err(error) = crate::agent_child_env::ensure_git_hooks(&storage_root) {
            crate::slog_warn!("config reload: could not install git hooks: {}", error);
        }
    }
}

/// The published config with only the live keys changed, and the names of the
/// changed keys by class.
#[derive(Debug, Clone)]
pub struct LiveApply {
    pub next: Config,
    pub applied: Vec<&'static str>,
    pub deferred: Vec<&'static str>,
}

/// Copy the live keys that differ from `published` out of `candidate`, and
/// list the deferred keys that differ from what the last configure resolved
/// (`connected`). Fields that describe the running process are never
/// compared: the candidate carries them over from the published config.
pub fn apply_live_config(published: &Config, candidate: &Config, connected: &Config) -> LiveApply {
    let mut next = published.clone();
    let mut applied = Vec::new();
    let mut deferred = Vec::new();

    macro_rules! live {
        ($key:expr, $($field:ident).+) => {
            if published.$($field).+ != candidate.$($field).+ {
                next.$($field).+ = candidate.$($field).+.clone();
                applied.push($key);
            }
        };
    }
    macro_rules! later {
        ($key:expr, $($field:ident).+) => {
            if connected.$($field).+ != candidate.$($field).+ {
                deferred.push($key);
            }
        };
    }

    // Edit pipeline.
    live!("format_on_edit", format_on_edit);
    live!("formatter_timeout_secs", formatter_timeout_secs);
    live!("type_checker_timeout_secs", type_checker_timeout_secs);
    live!("validate_on_edit", validate_on_edit);
    live!("formatter", formatter);
    live!("checker", checker);
    later!("edit_mode", hashline_enabled);
    // Tool surface and path restriction.
    later!("disabled_tools", disabled_tools);
    live!("restrict_to_project_root", restrict_to_project_root);
    live!("url_fetch_allow_private", url_fetch_allow_private);
    // Indexes, views, callgraph, standing roots.
    later!("indexes.trigram", indexes.trigram);
    later!("indexes.semantic", indexes.semantic);
    later!("indexes.callgraph", indexes.callgraph);
    later!("index.roots", index.roots);
    later!("views.enabled", views.enabled);
    live!("callgraph_chunk_size", callgraph_chunk_size);
    // Inspect.
    live!("inspect.enabled", inspect.enabled);
    live!("inspect.categories", inspect.categories);
    live!(
        "inspect.diagnostics_timeout_ms",
        inspect.diagnostics_timeout_ms
    );
    live!(
        "inspect.tier2_pass_timeout_ms",
        inspect.tier2_pass_timeout_ms
    );
    live!("inspect.duplicates.expected_mirrors", inspect.duplicates);
    // Idle, worktree, backup.
    live!("idle.root_ttl_minutes", idle.root_ttl_minutes);
    live!("lsp.idle_minutes", lsp_idle_minutes);
    live!("worktree.ram_overlay", worktree.ram_overlay);
    later!("backup.enabled", backup.enabled);
    later!("backup.max_depth", backup.max_depth);
    live!("backup.max_file_size", backup.max_file_size);
    // Sandbox: every spawn reads the snapshot its request pinned.
    live!("sandbox.enabled", sandbox.enabled);
    live!("sandbox.write_allow", sandbox.write_allow);
    live!("sandbox.read_deny", sandbox.read_deny);
    // Bash runtime.
    live!("bash.enabled", bash.enabled);
    live!("bash.rewrite", experimental_bash_rewrite);
    later!("bash.compress", experimental_bash_compress);
    later!("bash.background", experimental_bash_background);
    live!("bash.linux_scope", bash.linux_scope);
    live!("bash.disclaim_privacy", bash.disclaim_privacy);
    live!("bash.foreground_wait_window_ms", foreground_wait_window_ms);
    live!("bash.host_fallback", bash.host_fallback);
    live!("bash.runon_enabled", bash.runon_enabled);
    live!("bash.watch_sync_max_ms", bash.watch_sync_max_ms);
    live!("bash.worker_wait_max_ms", bash.worker_wait_max_ms);
    later!("bash.detach_on_user_message", bash.detach_on_user_message);
    live!("bash.db_schema_hints", bash.db_schema_hints);
    later!("bash.powershell_tool", bash.powershell_tool);
    live!(
        "bash.long_running_reminder_enabled",
        bash_long_running_reminder_enabled
    );
    live!(
        "bash.long_running_reminder_interval_ms",
        bash_long_running_reminder_interval_ms
    );
    // LSP.
    later!("lsp.servers", lsp_servers);
    later!("lsp.disabled", disabled_lsp);
    later!("experimental.lsp_ty", experimental_lsp_ty);
    live!("lsp.diagnostics_on_edit", diagnostics_on_edit);
    // Semantic search.
    later!("semantic.backend", semantic.backend);
    later!("semantic.model", semantic.model);
    later!("semantic.base_url", semantic.base_url);
    later!("semantic.api_key_env", semantic.api_key_env);
    later!("semantic.timeout_ms", semantic.timeout_ms);
    live!("semantic.query_timeout_ms", semantic.query_timeout_ms);
    live!("semantic.query_instruction", semantic.query_instruction);
    later!("semantic.max_batch_size", semantic.max_batch_size);
    later!("semantic.max_input_tokens", semantic.max_input_tokens);
    later!("semantic.max_files", semantic.max_files);
    later!("subc.connection_file", semantic.subc_connection_file);
    // Search reranking is read per request, so a change applies to the next
    // search.
    live!("search.rerank", search.rerank);
    // Integrations.
    later!("github.shim", github.shim);
    later!("github.read", github.read);
    later!("github.write", github.write);
    later!("gh_shim.binary_path", gh_shim.binary_path);
    // The OpenCode plugin reads the server it raises permission prompts on
    // once, at startup.
    later!("opencode.server_url", opencode.server_url);
    later!("opencode.server_password_env", opencode.server_password_env);
    live!("git.co_author", git.co_author);
    // Whether `runon` is offered is decided once per host process (the head
    // plugins build bash's arguments at startup), so a change waits for a
    // restart rather than refusing calls the surface still offers.
    later!("remote_exec", remote_exec);

    // `aft_search_registered` is derived from `disabled_tools`, which is
    // deferred, so it is never copied.
    LiveApply {
        next,
        applied,
        deferred,
    }
}

/// Compile-time completeness check for [`apply_live_config`]. Every `Config`
/// field, and every field of a struct whose keys are split between live and
/// deferred, is named here with no `..` rest pattern. Adding a field breaks
/// the build until it is classified above and listed here.
#[allow(dead_code)]
fn classification_is_exhaustive(config: &Config) {
    let Config {
        // Process state, carried over and never compared.
        project_root: _,
        validation_depth: _,
        checkpoint_ttl_hours: _,
        max_symbol_depth: _,
        max_background_bash_tasks: _,
        bash_permissions: _,
        search_index_max_file_size: _,
        lsp_paths_extra: _,
        lsp_auto_install_binaries: _,
        lsp_inflight_installs: _,
        storage_dir: _,
        harness: _,
        diagnostic_cache_size: _,
        aft_search_registered: _,
        // Classified in `apply_live_config`.
        formatter_timeout_secs: _,
        type_checker_timeout_secs: _,
        format_on_edit: _,
        hashline_enabled: _,
        validate_on_edit: _,
        formatter: _,
        checker: _,
        restrict_to_project_root: _,
        indexes,
        index: _,
        views: _,
        callgraph_chunk_size: _,
        experimental_bash_rewrite: _,
        experimental_bash_compress: _,
        experimental_bash_background: _,
        bash_long_running_reminder_enabled: _,
        bash_long_running_reminder_interval_ms: _,
        foreground_wait_window_ms: _,
        bash,
        sandbox,
        semantic,
        search: _,
        inspect,
        backup,
        worktree: _,
        github,
        gh_shim: _,
        opencode: _,
        git: _,
        remote_exec: _,
        experimental_lsp_ty: _,
        lsp_servers: _,
        disabled_lsp: _,
        diagnostics_on_edit: _,
        lsp_idle_minutes: _,
        url_fetch_allow_private: _,
        disabled_tools: _,
        idle,
    } = config;
    let crate::config::IndexesConfig {
        trigram: _,
        semantic: _,
        callgraph: _,
    } = indexes;
    let crate::config::BashConfig {
        enabled: _,
        host_fallback: _,
        runon_enabled: _,
        detach_on_user_message: _,
        db_schema_hints: _,
        watch_sync_max_ms: _,
        worker_wait_max_ms: _,
        linux_scope: _,
        disclaim_privacy: _,
        powershell_tool: _,
    } = bash;
    let crate::config::SandboxConfig {
        enabled: _,
        write_allow: _,
        read_deny: _,
    } = sandbox;
    let crate::config::SemanticBackendConfig {
        backend: _,
        model: _,
        base_url: _,
        api_key_env: _,
        timeout_ms: _,
        query_timeout_ms: _,
        query_instruction: _,
        max_batch_size: _,
        max_input_tokens: _,
        max_files: _,
        subc_connection_file: _,
        // Set by configure after resolving; process state.
        route_project_root: _,
        route_harness: _,
    } = semantic;
    let crate::config::InspectConfig {
        enabled: _,
        diagnostics_timeout_ms: _,
        tier2_pass_timeout_ms: _,
        categories: _,
        duplicates: _,
    } = inspect;
    let crate::config::BackupConfig {
        enabled: _,
        max_depth: _,
        max_file_size: _,
    } = backup;
    let crate::config::GithubConfig {
        shim: _,
        read: _,
        write: _,
    } = github;
    let crate::config::IdleConfig {
        root_ttl_minutes: _,
    } = idle;
}

/// A non-recursive watch on one config file's directory that requests a
/// reload when the file (or an editor's temporary sibling of it) changes.
///
/// When the directory does not exist yet, its parent is watched until it
/// appears. The watch runs on its own thread and stops shortly after the
/// value is dropped.
pub struct ConfigFileWatch {
    file: PathBuf,
    shutdown: Arc<AtomicBool>,
}

impl ConfigFileWatch {
    pub fn start(file: PathBuf, signal: Arc<ConfigReloadSignal>) -> Self {
        Self::start_with(file, Arc::new(move || signal.request()))
    }

    /// Watch `file` and call `on_change` for each relevant event.
    pub fn start_with(file: PathBuf, on_change: Arc<dyn Fn() + Send + Sync>) -> Self {
        let counters = crate::context::watcher_counters_for_root(file.parent().unwrap_or(&file));
        Self::start_with_counters(file, on_change, counters)
    }

    fn start_for_root(
        file: PathBuf,
        signal: Arc<ConfigReloadSignal>,
        counters: Arc<crate::context::WatcherCounters>,
    ) -> Self {
        Self::start_with_counters(file, Arc::new(move || signal.request()), counters)
    }

    fn start_with_counters(
        file: PathBuf,
        on_change: Arc<dyn Fn() + Send + Sync>,
        counters: Arc<crate::context::WatcherCounters>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_file = file.clone();
        let spawned = thread::Builder::new()
            .name("aft-config-watch".to_string())
            .spawn(move || {
                run_config_file_watch(thread_file, on_change, thread_shutdown, counters)
            });
        if let Err(error) = spawned {
            crate::slog_warn!(
                "config watch for {} could not start: {}",
                file.display(),
                error
            );
        }
        Self { file, shutdown }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }
}

impl Drop for ConfigFileWatch {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

const CONFIG_WATCH_POLL: Duration = Duration::from_millis(200);

fn run_config_file_watch(
    file: PathBuf,
    on_change: Arc<dyn Fn() + Send + Sync>,
    shutdown: Arc<AtomicBool>,
    counters: Arc<crate::context::WatcherCounters>,
) {
    use notify::RecursiveMode;
    #[cfg(not(target_os = "macos"))]
    use notify::Watcher;

    #[cfg(target_os = "macos")]
    type ConfigDirectoryWatcher = crate::watcher_backend::DirectoryWatcher;
    #[cfg(not(target_os = "macos"))]
    type ConfigDirectoryWatcher = notify::RecommendedWatcher;

    let Some(dir) = file.parent().map(Path::to_path_buf) else {
        return;
    };
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    #[cfg(target_os = "macos")]
    let mut watcher = ConfigDirectoryWatcher::new(tx, counters);
    #[cfg(not(target_os = "macos"))]
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(watcher) => watcher,
        Err(error) => {
            crate::slog_warn!("config watch for {} failed: {}", file.display(), error);
            return;
        }
    };
    #[cfg(not(target_os = "macos"))]
    let _ = counters;
    let mut attachment = DirAttachment::new(dir.clone());
    let attach = |watcher: &mut ConfigDirectoryWatcher, attachment: &mut DirAttachment| {
        attachment.attach(&mut |op| match op {
            WatchOp::Watch(path) => watcher
                .watch(path, RecursiveMode::NonRecursive)
                .map_err(|error| error.to_string()),
            WatchOp::Unwatch(path) => {
                let _ = watcher.unwatch(path);
                Ok(())
            }
        })
    };
    attach(&mut watcher, &mut attachment);

    while !shutdown.load(Ordering::Acquire) {
        match rx.recv_timeout(CONFIG_WATCH_POLL) {
            Ok(Ok(event)) => {
                handle_config_watch_event(&dir, &event, &mut attachment, on_change.as_ref());
            }
            Ok(Err(_)) => {
                attachment.force_reattach();
                on_change();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        // On every event and every tick: the directory appeared, vanished or
        // was replaced. Move the watch, and check the file, which may have
        // changed together with its directory.
        if attach(&mut watcher, &mut attachment) {
            on_change();
        }
    }
}

fn handle_config_watch_event(
    dir: &Path,
    event: &notify::Event,
    attachment: &mut DirAttachment,
    on_change: &dyn Fn(),
) {
    // Backends may report the resolved spelling of the directory (macOS
    // `/private/var` for `/var`), so compare both.
    let resolved_dir = std::fs::canonicalize(dir).ok();
    let is_dir = |candidate: &Path| candidate == dir || resolved_dir.as_deref() == Some(candidate);
    let relevant = event.need_rescan()
        || event.paths.is_empty()
        || event.paths.iter().any(|path| {
            (path.parent().is_some_and(is_dir) && is_config_file_name(path)) || is_dir(path)
        });
    // FSEvents drop sentinels name the watched root even when it has not
    // changed. Re-reading our state is required; restarting the stream is not.
    // Only directory lifetime events need the inode-reuse safeguard. Identity
    // checks on every tick still catch replacements after lost events.
    let directory_lifetime_changed = !event.need_rescan()
        && matches!(
            event.kind,
            notify::EventKind::Create(_)
                | notify::EventKind::Remove(_)
                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
    if let Some(watched) = attachment.watched().filter(|_| directory_lifetime_changed) {
        let resolved_watched = std::fs::canonicalize(watched).ok();
        if event
            .paths
            .iter()
            .any(|path| path == watched || resolved_watched.as_deref() == Some(path.as_path()))
        {
            attachment.force_reattach();
        }
    }
    if relevant {
        on_change();
    }
}

/// A watch request made by [`DirAttachment`].
pub(crate) enum WatchOp<'a> {
    Watch(&'a Path),
    Unwatch(&'a Path),
}

/// Which directory a config file watch is attached to: the file's own
/// directory, or its parent while that directory does not exist, together
/// with the directory's identity when it was attached. Backends that watch
/// by inode keep following a directory that was renamed aside, so a
/// directory replaced under the same name must be attached again.
pub(crate) struct DirAttachment {
    dir: PathBuf,
    watching: Option<(PathBuf, Option<DirIdentity>)>,
    /// Set by [`Self::force_reattach`]; the next attach re-watches.
    stale: bool,
}

impl DirAttachment {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            watching: None,
            stale: false,
        }
    }

    /// The directory currently watched, if any.
    pub(crate) fn watched(&self) -> Option<&Path> {
        self.watching.as_ref().map(|(path, _)| path.as_path())
    }

    /// Make the next [`Self::attach`] watch again even if the directory's
    /// identity looks unchanged. Used when a lifetime event names the watched
    /// directory itself or the backend reports an error: a directory deleted
    /// and recreated can get the same inode back (ext4 reuses them), which
    /// the identity check alone would miss.
    pub(crate) fn force_reattach(&mut self) {
        self.stale = true;
    }

    /// Attach, move or re-attach the watch as the directories on disk
    /// require. Returns true when it (re)attached to the file's own directory,
    /// so the caller should check the file.
    pub(crate) fn attach(&mut self, op: &mut dyn FnMut(WatchOp<'_>) -> Result<(), String>) -> bool {
        let target = if self.dir.is_dir() {
            self.dir.clone()
        } else {
            match self.dir.parent().filter(|parent| parent.is_dir()) {
                Some(parent) => parent.to_path_buf(),
                None => return false,
            }
        };
        let identity = dir_identity(&target);
        if !self.stale
            && self
                .watching
                .as_ref()
                .is_some_and(|(path, seen)| *path == target && *seen == identity)
        {
            return false;
        }
        self.stale = false;
        if let Some((previous, _)) = self.watching.take() {
            let _ = op(WatchOp::Unwatch(&previous));
        }
        match op(WatchOp::Watch(&target)) {
            Ok(()) => {
                // The directory may have been replaced between the identity
                // read and the watch; the next tick then attaches again.
                if dir_identity(&target) != identity {
                    let _ = op(WatchOp::Unwatch(&target));
                    return false;
                }
                let moved_to_dir = target == self.dir;
                self.watching = Some((target, identity));
                moved_to_dir
            }
            Err(error) => {
                crate::slog_warn!("config watch on {} failed: {}", target.display(), error);
                false
            }
        }
    }
}

/// A directory's device and inode (on Windows, volume serial number and
/// file index), which change when it is replaced.
type DirIdentity = (u64, u64);

#[cfg(unix)]
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// On Windows, the volume serial number and the 64-bit file index, which is
/// what an inode is on Unix. Creation time would not do: NTFS file-name
/// tunneling gives a name reused within about 15 s the creation time of the
/// entry that last had it, so a directory renamed aside and recreated looks
/// unchanged by that measure.
#[cfg(windows)]
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
    // the remaining arguments are plain flags and null pointers, and the
    // returned handle is checked before use and closed below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            // Required to open a directory handle.
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value (plain
    // integers and FILETIME structs); the call only writes into it.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is a valid open handle and `info` a valid out pointer.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
    // SAFETY: `handle` came from CreateFileW above and is closed exactly once.
    unsafe {
        CloseHandle(handle);
    }
    ok.then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn dir_identity(_path: &Path) -> Option<DirIdentity> {
    None
}

/// Process-level watch on the user config file for the daemon. On a change it
/// asks every live root to reload; roots that are not bound pick the edit up
/// at their next bind instead.
pub fn start_process_user_config_watch(
    path: PathBuf,
    roots: impl Fn() -> Vec<Arc<AppContext>> + Send + Sync + 'static,
) -> ConfigFileWatch {
    set_process_user_config_path(path.clone());
    ConfigFileWatch::start_with(path, Arc::new(move || request_reload_for_roots(&roots())))
}

/// Ask each root to re-read its config files once the debounce has passed.
pub fn request_reload_for_roots(roots: &[Arc<AppContext>]) {
    for ctx in roots {
        ctx.config_live().signal.request();
    }
}

#[cfg(test)]
#[path = "config_live_tests.rs"]
mod tests;
