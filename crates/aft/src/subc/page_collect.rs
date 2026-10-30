//! `page.collect`, `page.read` and `page.release`: core's management
//! operations for sealed HTML page bundles (see `crate::page_collect`).
//!
//! Only core (`prefrontal-core`) may call them. A collection reads under
//! the read policy of one agent session's live tool route: the same bind
//! root, trust class, config read_deny and protected paths, AFT storage
//! floor, saved grants and confinement its `read` tool applies right now:
//! the per-call setup comes from `call_policy::with_tool_call_policy`, the
//! same function the tool-call path uses, and every path that passes the
//! storage floor is then judged by [`AppContext::validate_read_path`], the
//! check `read` itself makes. A question the policy would ask the user is
//! never asked here; such a path counts as denied.

use super::*;
use crate::commands::file_read_exact::{self, OpenedFile, ReadExactRefusal};
use crate::page_collect::snapshot::{BeginError, SnapshotOwner, SnapshotStore};
use crate::page_collect::{self as page, Limits, PageSource, ReadFailure};

/// The module whose management principal may call these operations.
const CORE_MODULE_ID: &str = "prefrontal-core";

pub(super) const SESSION_NOT_BOUND: &str = "session_not_bound";
pub(super) const SESSION_ROUTE_AMBIGUOUS: &str = "session_route_ambiguous";

pub(super) fn is_page_operation(operation: &str) -> bool {
    matches!(
        operation,
        page::COLLECT_OPERATION | page::READ_OPERATION | page::RELEASE_OPERATION
    )
}

fn caller_is_core(principal: Option<&Principal>) -> bool {
    matches!(principal, Some(Principal::Reserved { module_id }) if module_id == CORE_MODULE_ID)
}

fn response_id(operation: &str) -> String {
    format!("management-{}", operation.replace('.', "-"))
}

fn refusal(operation: &str, error: subc_protocol::ErrorBody) -> Response {
    let detail = error.detail.unwrap_or_else(|| json!({}));
    Response::error_with_data(
        response_id(operation),
        &error.code,
        error.message,
        json!({ "detail": detail }),
    )
}

fn caller_refused(operation: &str) -> Response {
    refusal(
        operation,
        subc_protocol::ErrorBody::new(
            "page_caller_refused",
            format!("{operation} serves only core's management principal"),
        ),
    )
}

fn invalid(operation: &str, message: impl Into<String>) -> Response {
    refusal(
        operation,
        subc_protocol::ErrorBody::new("invalid_request", message.into()),
    )
}

/// The live classic tool routes of `{harness, session}`, ordered by channel.
/// Codemode run routes are left out: their session is configuration, never
/// authority (see `codemode`).
pub(super) fn session_routes(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    harness: &str,
    session: &str,
) -> Vec<(RouteChannel, RouteIdentity)> {
    let mut found = routes
        .iter()
        .filter(|(_, identity)| {
            identity.harness == harness
                && identity.session == session
                && identity.run_owner.is_none()
                && identity.call_mode == subc_protocol::call_mode::CallMode::Classic
        })
        .map(|(channel, identity)| (*channel, identity.clone()))
        .collect::<Vec<_>>();
    found.sort_by_key(|(channel, _)| (channel.channel, channel.epoch));
    found
}

/// Pick the route whose read policy a collection uses, by the same rule a
/// codemode call uses to pick its head agent's route: prefer routes bound
/// at the root the page is in (when the entry is absolute), then require
/// every remaining route to agree on access settings
/// (`codemode::head_access_differences`) and, when there are several, on
/// their saved restrictions. Never a union of routes.
pub(super) fn choose_route(
    candidates: Vec<(RouteChannel, RouteIdentity)>,
    entry: &Path,
    mut saved_policy: impl FnMut(&RouteIdentity) -> codemode::SavedPolicyRead,
) -> Result<RouteIdentity, subc_protocol::ErrorBody> {
    let mut candidates = candidates;
    if candidates.is_empty() {
        return Err(subc_protocol::ErrorBody::new(
            SESSION_NOT_BOUND,
            "no live tool route of this session is bound to AFT",
        ));
    }
    if entry.is_absolute()
        && candidates
            .iter()
            .any(|(_, identity)| entry.starts_with(&identity.project_root))
    {
        candidates.retain(|(_, identity)| entry.starts_with(&identity.project_root));
    }
    let (first_channel, first) = candidates[0].clone();
    let ambiguous = |other: RouteChannel, fields: &[&str]| {
        subc_protocol::ErrorBody::new(
            SESSION_ROUTE_AMBIGUOUS,
            format!(
                "this session's routes {} and {} disagree on access settings: {}",
                first_channel.channel,
                other.channel,
                fields.join(", ")
            ),
        )
        .with_detail(json!({
            "fields": fields,
            "channels": [first_channel.channel, other.channel],
        }))
    };
    for (channel, identity) in &candidates[1..] {
        let fields = codemode::head_access_differences(&first, identity);
        if !fields.is_empty() {
            return Err(ambiguous(*channel, &fields));
        }
    }
    if candidates.len() > 1 {
        let first_policy = saved_policy(&first)?;
        for (channel, identity) in &candidates[1..] {
            if saved_policy(identity)? != first_policy {
                return Err(ambiguous(*channel, &["saved_policy"]));
            }
        }
    }
    Ok(first)
}

/// How a session reader opens a checked file: below `walk_root`, never
/// following a symlink component. Runs on a bounded I/O helper thread.
pub(super) type OpenFile =
    Arc<dyn Fn(&Path, &Path) -> Result<OpenedFile, ReadExactRefusal> + Send + Sync>;

/// Runs on the bounded I/O helper thread just before a checked file's bytes
/// are read, with that file's path.
pub(super) type BeforeRead = Arc<dyn Fn(&Path) + Send + Sync>;

/// How a session reader opens files, and a test's view into its steps.
#[derive(Clone)]
pub(super) struct ReadHooks<'a> {
    pub(super) open: OpenFile,
    /// Runs with each spelled path after its policy check and before it is
    /// resolved, so a test can swap what the spelling points at in between.
    pub(super) after_check: Option<&'a dyn Fn(&Path)>,
    /// Lets a test stall a read the way a hung network filesystem would.
    pub(super) before_read: Option<BeforeRead>,
}

impl ReadHooks<'_> {
    /// The hooks production uses: the no-follow open, nothing else.
    pub(super) fn production() -> Self {
        ReadHooks {
            open: Arc::new(|root, target| file_read_exact::open_no_follow(root, target)),
            after_check: None,
            before_read: None,
        }
    }
}

/// Reads page files as one agent session's `read` tool may. Must run inside
/// `call_policy::with_tool_call_policy` for that session's route.
///
/// Every step that can block on the filesystem (the policy checks' stats,
/// resolution, the storage-floor stat, the open, and the read) runs on the
/// bounded I/O helpers under a budget that ends at the read's deadline. A
/// file whose I/O is still stuck then is reported as past the deadline and
/// the collection goes on; the stuck helper keeps the open file until its
/// call returns.
pub(super) struct SessionReader<'a> {
    pub(super) ctx: &'a AppContext,
    pub(super) session: &'a str,
    pub(super) request_id: &'a str,
    pub(super) hooks: ReadHooks<'a>,
}

impl SessionReader<'_> {
    fn policy_allows(&self, path: &Path) -> bool {
        self.ctx
            .validate_read_path(self.request_id, self.session, path)
            .is_ok()
    }

    /// [`storage_floor`] on a bounded helper.
    fn floor(&self, path: &Path, deadline: Instant) -> Result<bool, ReadFailure> {
        crate::bounded_io::run(path, Some(deadline), |path| Ok(storage_floor(&path)))
            .map_err(|_| ReadFailure::Deadline)
    }
}

/// `failure`, unless the read's deadline has passed: a check that failed
/// because its I/O ran out of time is reported as the deadline.
fn unless_late(deadline: Instant, failure: ReadFailure) -> ReadFailure {
    if Instant::now() >= deadline {
        ReadFailure::Deadline
    } else {
        failure
    }
}

/// The storage floor, checked before any open: whether `path` is in AFT's
/// storage, is named like a SQLite file, or is a path a live database
/// connection in this process registered. Decided by name and stat alone,
/// because opening and then closing a descriptor on a live SQLite file
/// drops this process's SQLite locks on it. (A handle that turns out to be
/// a live SQLite file anyway, such as an unregistered hard link, is refused
/// after the open and kept open rather than closed; see `OpenedFile`.)
fn storage_floor(path: &Path) -> bool {
    crate::project_content::is_excluded(path)
}

/// A file opened on a helper: the handle, its metadata, and where it is.
type Opened = (OpenedFile, std::fs::Metadata, PathBuf);

impl PageSource for SessionReader<'_> {
    fn read(&mut self, path: &Path, limit: u64, deadline: Instant) -> Result<Vec<u8>, ReadFailure> {
        use std::io::Read;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ReadFailure::Deadline);
        }
        // This file's own budget: its stuck I/O ends at the deadline and does
        // not fail the I/O of files read after it.
        let _io = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(remaining)));
        if self.floor(path, deadline)? {
            return Err(ReadFailure::Denied);
        }
        let checked = self
            .ctx
            .validate_read_path(self.request_id, self.session, path)
            .map_err(|_| unless_late(deadline, ReadFailure::Denied))?;
        if let Some(after_check) = self.hooks.after_check {
            after_check(path);
        }
        // Resolve again, now: a symlink in the spelling may have been
        // re-pointed since the check. The location about to be opened is the
        // one judged below, and the open refuses any symlink swapped in after.
        let resolved = match crate::bounded_io::canonicalize(&checked) {
            Ok(resolved) => crate::inspect::job::normalize_path(&resolved),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(unless_late(deadline, ReadFailure::Missing))
            }
            Err(_) => return Err(unless_late(deadline, ReadFailure::Denied)),
        };
        // The spelled-path floor check above cannot see where a re-pointed
        // symlink now leads; this one guards the open itself.
        if self.floor(&resolved, deadline)? {
            return Err(ReadFailure::Denied);
        }
        if resolved != crate::inspect::job::normalize_path(&checked)
            && !self.policy_allows(&resolved)
        {
            return Err(unless_late(deadline, ReadFailure::Denied));
        }
        // The walk starts at the filesystem root: the resolved path has no
        // symlink in it, so the component-by-component no-follow open lands
        // exactly there or fails.
        let walk_root = resolved
            .ancestors()
            .last()
            .unwrap_or(&resolved)
            .to_path_buf();
        let open = Arc::clone(&self.hooks.open);
        let opened = crate::bounded_io::run(&resolved, Some(deadline), move |resolved| {
            let file = match open(&walk_root, &resolved) {
                Ok(file) => file,
                Err(ReadExactRefusal::NotFound) => return Ok(Err(ReadFailure::Missing)),
                Err(ReadExactRefusal::AccessDenied) => return Ok(Err(ReadFailure::Denied)),
                Err(_) => return Ok(Err(ReadFailure::Missing)),
            };
            let Ok(metadata) = file.metadata() else {
                return Ok(Err(ReadFailure::Missing));
            };
            if crate::db::file_identity::is_live_sqlite_handle(&metadata) {
                return Ok(Err(ReadFailure::Denied));
            }
            // Where the handle really is, judged below on the caller's thread.
            let Ok(opened) = file_read_exact::opened_location(&file) else {
                return Ok(Err(ReadFailure::Denied));
            };
            let opened = crate::inspect::job::normalize_path(&opened);
            Ok(Ok::<Opened, ReadFailure>((file, metadata, opened)))
        })
        .map_err(|_| ReadFailure::Deadline)??;
        let (file, metadata, opened) = opened;
        // Where the handle really is must pass the same policy again.
        if self.floor(&opened, deadline)? || (opened != resolved && !self.policy_allows(&opened)) {
            return Err(unless_late(deadline, ReadFailure::Denied));
        }
        if !metadata.is_file() {
            return Err(ReadFailure::Missing);
        }
        if metadata.len() > limit {
            return Err(ReadFailure::TooLarge {
                size: metadata.len(),
            });
        }
        let before_read = self.hooks.before_read.clone();
        let bytes = crate::bounded_io::run(&resolved, Some(deadline), move |resolved| {
            if let Some(before_read) = before_read {
                before_read(&resolved);
            }
            let mut file = file;
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            Ok((&mut *file)
                .take(limit + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
                .map_err(|_| ReadFailure::Missing))
        })
        .map_err(|_| ReadFailure::Deadline)??;
        if bytes.len() as u64 > limit {
            return Err(ReadFailure::TooLarge {
                size: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectParams {
    harness: String,
    session: String,
    entry: String,
    #[serde(default)]
    limits: Option<Value>,
}

/// Everything `page.collect` needs from the frame loop, copied when the
/// request arrives so the collection runs off the loop.
pub(super) struct CollectCall {
    pub(super) params: Value,
    pub(super) principal: Option<Principal>,
    pub(super) routes: HashMap<RouteChannel, RouteIdentity>,
    pub(super) executor: Arc<Executor>,
    pub(super) store: Arc<SnapshotStore>,
}

/// Handle `page.collect` on a blocking thread.
pub(super) fn collect(call: CollectCall) -> Response {
    collect_with_hooks(call, ReadHooks::production())
}

pub(super) fn collect_with_hooks(call: CollectCall, hooks: ReadHooks<'_>) -> Response {
    const OP: &str = page::COLLECT_OPERATION;
    let started = Instant::now();
    if !caller_is_core(call.principal.as_ref()) {
        return caller_refused(OP);
    }
    let params = match serde_json::from_value::<CollectParams>(call.params) {
        Ok(params) => params,
        Err(error) => return invalid(OP, format!("page.collect params: {error}")),
    };
    let limits = match Limits::from_request(params.limits.as_ref()) {
        Ok(limits) => limits,
        Err(message) => return invalid(OP, message),
    };
    let entry_spelling = PathBuf::from(&params.entry);
    let candidates = session_routes(&call.routes, &params.harness, &params.session)
        .into_iter()
        .filter_map(|(channel, identity)| {
            let ctx = call.executor.actor_context(&identity.root)?;
            Some((channel, identity, ctx))
        })
        .collect::<Vec<_>>();
    let policies = codemode::SavedPolicies::read(
        candidates
            .iter()
            .map(|(_, identity, ctx)| (identity.clone(), Arc::clone(ctx)))
            .collect(),
    );
    let identity = match choose_route(
        candidates
            .into_iter()
            .map(|(channel, identity, _)| (channel, identity))
            .collect(),
        &entry_spelling,
        |identity| {
            let ctx = call
                .executor
                .actor_context(&identity.root)
                .ok_or_else(remote_policy::policy_unavailable)?;
            policies
                .get(&ctx, identity)
                .unwrap_or_else(|| Err(remote_policy::policy_unavailable()))
        },
    ) {
        Ok(identity) => identity,
        Err(error) => return refusal(OP, error),
    };
    let Some(ctx) = call.executor.actor_context(&identity.root) else {
        return refusal(
            OP,
            subc_protocol::ErrorBody::new(SESSION_NOT_BOUND, "the session's route was unbound"),
        );
    };
    if !crate::tool_gate::catalog_keeps("read", &identity.disabled_tools) {
        return refusal(
            OP,
            page::CollectRefusal::EntryUnreadable {
                reason: "read_disabled",
            }
            .into_error(),
        );
    }
    let entry = crate::path_identity::lexical_normalize(&if entry_spelling.is_absolute() {
        entry_spelling
    } else {
        identity.project_root.join(entry_spelling)
    });
    let owner = SnapshotOwner {
        harness: params.harness.clone(),
        session: params.session.clone(),
    };
    let mut builder = match call.store.begin(owner) {
        Ok(builder) => builder,
        Err(BeginError::Busy(busy)) => {
            return refusal(
                OP,
                subc_protocol::ErrorBody::new(
                    "busy",
                    format!(
                        "{} page snapshots are already live; retry later",
                        page::snapshot::MAX_LIVE
                    ),
                )
                .with_detail(json!({ "retry_after_ms": busy.retry_after_ms })),
            )
        }
        Err(BeginError::Io(error)) => {
            return refusal(OP, page::CollectRefusal::Storage(error).into_error())
        }
    };
    // The collection's own checks (the confinement admission checks the
    // page's spelling) get at least a second. Each file read then runs
    // under its own budget, which ends at that file's deadline.
    let budget = crate::bounded_io::Budget::after(
        Duration::from_millis(limits.deadline_ms).max(Duration::from_secs(1)),
    );
    static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    // Root restriction and confinement are keyed by request id.
    let request_id = format!(
        "page-collect-{}",
        NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    // The page is read exactly as the session's `read` tool reads: through
    // the same per-call policy setup, as a `read` of the page's spelling.
    let arguments = json!({ "path": params.entry });
    let ask_route = access_grants::ask_route_for_call(&identity, "read", &arguments);
    let _io = crate::bounded_io::enter(Some(budget));
    let collected = call_policy::with_tool_call_policy(
        &ctx,
        &identity,
        &request_id,
        ask_route,
        identity.trust,
        "read",
        arguments,
        |_| {
            let mut reader = SessionReader {
                ctx: &ctx,
                session: &identity.session,
                request_id: &request_id,
                hooks,
            };
            page::collect(&entry, &limits, started, &mut reader, &mut builder)
        },
        |collected| collected,
    );
    match collected {
        // The confinement admission refused before any page file was
        // read: its policy could not be read, or the page's spelling is
        // outside the confined root.
        Err(response) if response.data["code"] == "confine_policy_unavailable" => refusal(
            OP,
            subc_protocol::ErrorBody::new(
                "confine_policy_unavailable",
                response.data["message"].as_str().unwrap_or_default(),
            ),
        ),
        Err(_) => refusal(
            OP,
            page::CollectRefusal::EntryUnreadable { reason: "denied" }.into_error(),
        ),
        Ok(Err(error)) => refusal(OP, error.into_error()),
        Ok(Ok(collected)) => {
            let (snapshot, expires_at_ms) = builder.commit();
            Response::success(response_id(OP), collected.reply(&snapshot, expires_at_ms))
        }
    }
}

impl page::CollectRefusal {
    fn into_error(self) -> subc_protocol::ErrorBody {
        subc_protocol::ErrorBody::new(self.code(), self.message()).with_detail(self.detail())
    }
}

/// The outcome of `page.read`: raw bytes, sent as a binary frame, or a
/// JSON refusal.
pub(super) enum ReadReply {
    Bytes(Vec<u8>),
    Refused(Response),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    harness: String,
    session: String,
    snapshot: String,
    path: String,
    #[serde(default)]
    offset: u64,
    length: u64,
}

pub(super) fn read(
    params: Value,
    principal: Option<&Principal>,
    store: &SnapshotStore,
) -> ReadReply {
    const OP: &str = page::READ_OPERATION;
    if !caller_is_core(principal) {
        return ReadReply::Refused(caller_refused(OP));
    }
    let params = match serde_json::from_value::<ReadParams>(params) {
        Ok(params) => params,
        Err(error) => return ReadReply::Refused(invalid(OP, format!("page.read params: {error}"))),
    };
    // A snapshot answers only to the harness/session pair it was collected
    // for.
    let owner = SnapshotOwner {
        harness: params.harness,
        session: params.session,
    };
    match store.read(
        &params.snapshot,
        &owner,
        &params.path,
        params.offset,
        params.length,
    ) {
        Ok(bytes) => ReadReply::Bytes(bytes),
        Err(refusal_kind) => {
            let message = match refusal_kind {
                page::snapshot::ReadRefusal::Expired => {
                    "the snapshot expired or was released".to_string()
                }
                page::snapshot::ReadRefusal::NotInSnapshot => {
                    "the path is not a file of this snapshot".to_string()
                }
                page::snapshot::ReadRefusal::BadRange => format!(
                    "offset must be within the file and length between 1 and {}",
                    page::MAX_READ_LENGTH
                ),
            };
            ReadReply::Refused(refusal(
                OP,
                subc_protocol::ErrorBody::new(refusal_kind.code(), message),
            ))
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseParams {
    harness: String,
    session: String,
    snapshot: String,
}

pub(super) fn release(
    params: Value,
    principal: Option<&Principal>,
    store: &SnapshotStore,
) -> Response {
    const OP: &str = page::RELEASE_OPERATION;
    if !caller_is_core(principal) {
        return caller_refused(OP);
    }
    let params = match serde_json::from_value::<ReleaseParams>(params) {
        Ok(params) => params,
        Err(error) => return invalid(OP, format!("page.release params: {error}")),
    };
    let owner = SnapshotOwner {
        harness: params.harness,
        session: params.session,
    };
    let released = store.release(&params.snapshot, &owner);
    Response::success(
        response_id(OP),
        json!({ "snapshot": params.snapshot, "released": released }),
    )
}

#[cfg(test)]
#[path = "page_collect_tests.rs"]
mod tests;
