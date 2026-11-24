//! Incomplete typed agent-data protocol core. Catalog admission remains disabled
//! until native identity lookup, the daemon session fence and crash-atomic
//! lifecycle publication are implemented; missing dependencies refuse access.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use contract::{
    errors::{ErrorBody, RefusalCode},
    refs::{Ref, Refs},
    request::Request,
    step::Step,
    Op,
};
use cortexkit_role_agent_data as contract;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::door::{read_verified_cut, SessionDoor, PAGE_LIMIT};
use super::snapshot::{MOVE_LIMIT, SESSION_LIMIT};
use super::{DropCounts, ProjectRegistry, SessionSnapshot, SnapshotGroup};

#[derive(Debug, Clone)]
pub struct ResolvedSession {
    pub registered_ref: String,
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
    pub storage_harness: crate::harness::Harness,
}

impl ResolvedSession {
    pub fn session_ref(&self) -> Result<String, String> {
        if !contract::refs::is_session_ref(&self.registered_ref) {
            return Err("registered engram container identity is not a sessionRef".into());
        }
        Ok(self.registered_ref.clone())
    }
}

#[derive(Debug)]
pub enum BindingFailure {
    NotBindable,
    Unavailable(String),
}

/// Only registered, attested identity may supply this mapping. A bare session
/// string or a folder name never establishes ownership of another store.
pub trait RefResolver: Send + Sync {
    fn resolve(&self, reference: &Ref) -> Result<ResolvedSession, BindingFailure>;
}

pub struct UnbindableRefs;
impl RefResolver for UnbindableRefs {
    fn resolve(&self, _reference: &Ref) -> Result<ResolvedSession, BindingFailure> {
        Err(BindingFailure::NotBindable)
    }
}

/// A `SessionActivity::acquire` implementation must return a `SessionBarrier`
/// that excludes file/safety mutations, history pruning, bash launches and
/// approvals for the selected sessions until drop. Freeze and release must
/// survive process restart and refuse subsequent local admission. Unknown
/// activity is never idle. The daemon does not yet implement this fence, so its
/// production fallback refuses acquisition and discovery must remain disabled.
pub trait SessionBarrier {
    fn busy(&self, session: &ResolvedSession) -> contract::busy::BusyEntry;
    fn freeze(&mut self, cut: &str, sessions: &[ResolvedSession]) -> Result<(), String>;
    fn abort(&mut self, cut: &str, sessions: &[ResolvedSession]) -> Result<(), String>;
    fn release(
        &mut self,
        cut: &str,
        sessions: &[ResolvedSession],
        holder: &str,
    ) -> Result<bool, String>;
    fn activate(&mut self, cut: &str, sessions: &[ResolvedSession]) -> Result<(), String>;
}

pub trait SessionActivity: Send + Sync {
    fn acquire(&self, sessions: &[ResolvedSession])
        -> Result<Box<dyn SessionBarrier + '_>, String>;
}

pub struct UnavailableActivity;
impl SessionActivity for UnavailableActivity {
    fn acquire(
        &self,
        _sessions: &[ResolvedSession],
    ) -> Result<Box<dyn SessionBarrier + '_>, String> {
        Err("session-wide activity admission is not connected".into())
    }
}

/// All calls occur under the host's session barrier. Publication must be
/// repeatable with stable entry IDs; a partially published generation must stay
/// hidden behind that barrier until activation recovers or abort rolls it back.
pub trait SessionStores: Send + Sync {
    fn generation(&self, session: &ResolvedSession) -> Result<String, String>;
    fn snapshot(
        &self,
        session: &ResolvedSession,
        drops: &mut DropCounts,
    ) -> Result<SessionSnapshot, String>;
    fn publish(
        &self,
        session: &ResolvedSession,
        projection: &[(SnapshotGroup, Vec<PathBuf>)],
    ) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedReply {
    params: Value,
    reply: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stream {
    session_ref: String,
    digest: String,
    end: u64,
    floor: u64,
    file: String,
    local_baseline: String,
    storage_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    step: Step,
    refs: Option<Refs>,
    replies: BTreeMap<String, SavedReply>,
    streams: Vec<Stream>,
    drops: DropCounts,
}
impl Default for Journal {
    fn default() -> Self {
        Self {
            step: Step::None,
            refs: None,
            replies: BTreeMap::new(),
            streams: Vec::new(),
            drops: DropCounts::default(),
        }
    }
}

pub struct Participant {
    directory: PathBuf,
    resolver: Arc<dyn RefResolver>,
    activity: Arc<dyn SessionActivity>,
    stores: Arc<dyn SessionStores>,
    registry: Arc<dyn ProjectRegistry>,
    door: Arc<dyn SessionDoor>,
}

impl Participant {
    pub fn new(
        directory: PathBuf,
        resolver: Arc<dyn RefResolver>,
        activity: Arc<dyn SessionActivity>,
        stores: Arc<dyn SessionStores>,
        registry: Arc<dyn ProjectRegistry>,
        door: Arc<dyn SessionDoor>,
    ) -> Self {
        Self {
            directory,
            resolver,
            activity,
            stores,
            registry,
            door,
        }
    }

    /// Admission precedes decoding, locking, identity resolution and disk reads.
    pub fn handle(
        &self,
        stamped_principal: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value, ErrorBody> {
        if !contract::caller_admitted(stamped_principal) {
            return Err(error(
                RefusalCode::CallerForbidden,
                "only entorhinal may move AFT data",
            ));
        }
        let request = Request::decode(method, params)?;
        let op = request.op();
        let result = self.handle_admitted(&request);
        if let Err(ref refusal) = result {
            debug_assert!(contract::errors::may_refuse(op, &refusal.code));
            let _ = contract::errors::driver_action_for(op, &refusal.code);
        }
        result
    }

    fn handle_admitted(&self, request: &Request) -> Result<Value, ErrorBody> {
        let op = request.op();
        if op == Op::Describe {
            return Ok(json!({"ref_kinds":["opencode-session","pi-session"],
                "stores":[{"name":"undo","sync_class":"agent"},{"name":"checkpoints","sync_class":"agent"},
                {"name":"processes, replay ledgers, indexes and permissions","sync_class":"machine_local"}],"external":false}));
        }
        crate::private_storage::create_dir_all(&self.directory).map_err(unavailable)?;
        let _lock = crate::fs_lock::acquire(&self.directory.join("participant.lock"))
            .map_err(unavailable)?;
        let cut = request.cut_id().map(|cut| cut.as_str()).unwrap_or("");
        let mut journal = if cut.is_empty() {
            Journal::default()
        } else {
            self.load(cut)?
        };
        if op.changes_state() {
            if let Some(saved) = journal.replies.get(request.op().method()) {
                let first = Request::decode(request.op().method(), &saved.params)
                    .map_err(|_| unavailable("corrupt request journal"))?;
                match contract::idempotency::compare(&first, request) {
                    contract::idempotency::Repeat::Replay => return Ok(saved.reply.clone()),
                    contract::idempotency::Repeat::Reused => {
                        return Err(error(
                            RefusalCode::IdReused,
                            "cut operation was already used for another request",
                        ))
                    }
                    _ => return Err(unavailable("corrupt idempotency key")),
                }
            }
        }
        if op == Op::Status {
            return Ok(json!({"step":journal.step}));
        }
        let refs = request
            .refs()
            .ok_or_else(|| unavailable("request has no refs"))?;
        if op.changes_state() {
            contract::step::advance(op, journal.step).map_err(|refusal| match refusal {
                contract::step::StepRefusal::CutUnknown => {
                    error(RefusalCode::CutUnknown, "cut has no preceding state")
                }
                contract::step::StepRefusal::InvalidState { step } => invalid_state(step, None),
            })?;
            contract::step::check_ref_set(op, journal.step, journal.refs.as_ref(), refs)
                .map_err(|_| invalid_state(journal.step, journal.refs.as_ref()))?;
        }
        // An unseen abort must succeed even when no identity was ever bindable.
        if op == Op::Abort && journal.step == Step::None {
            return self.finish(cut, request, journal, Step::Aborted, json!({}));
        }
        let mut sessions = Vec::new();
        for reference in refs.as_slice() {
            let session = self.resolver.resolve(reference).map_err(|failure| {
                if let BindingFailure::Unavailable(reason) = failure {
                    return unavailable(reason);
                }
                if matches!(op, Op::Busy | Op::Prepare | Op::Export | Op::Release) {
                    let mut body = error(
                        RefusalCode::UnknownRef,
                        "registered session identity is not bindable",
                    );
                    body.detail =
                        Some(json!({"ref_kind":reference.ref_kind,"ref":reference.r#ref}));
                    body
                } else {
                    error(
                        RefusalCode::InvalidRequest,
                        "registered session identity is not bindable",
                    )
                }
            })?;
            if session.registered_ref != reference.r#ref
                || matches!(session.session.as_str(), "" | "__default__" | "unbound")
            {
                return Err(unavailable("registered session identity is incomplete"));
            }
            sessions.push(session);
        }
        let mut barrier = self.activity.acquire(&sessions).map_err(unavailable)?;
        if op == Op::Busy {
            return serde_json::to_value(contract::busy::BusyReply::new(
                sessions
                    .iter()
                    .map(|session| barrier.busy(session))
                    .collect(),
            ))
            .map_err(unavailable);
        }
        if matches!(op, Op::Prepare | Op::Import | Op::Activate) {
            let mut busy = Vec::new();
            for (reference, session) in refs.as_slice().iter().zip(&sessions) {
                let entry = barrier.busy(session);
                if entry.state != contract::busy::BusyState::Idle {
                    busy.push(json!({"ref_kind":reference.ref_kind,"ref":reference.r#ref,"reason_code":"mutation_active","reason":"session activity is busy or unknown"}));
                }
            }
            if !busy.is_empty() {
                let mut body = error(RefusalCode::Busy, "sessions cannot be cut while active");
                body.detail = Some(json!({"refs":busy}));
                return Err(body);
            }
        }
        match op {
            Op::Prepare => {
                let mut streams = Vec::new();
                let mut items = Vec::new();
                let mut budget = super::snapshot::CarryBudget::default();
                for (index, (reference, session)) in
                    refs.as_slice().iter().zip(&sessions).enumerate()
                {
                    let snapshot = self
                        .stores
                        .snapshot(session, &mut journal.drops)
                        .map_err(|reason| error(RefusalCode::NotExportable, reason))?;
                    let entries = snapshot
                        .groups
                        .iter()
                        .map(|group| group.files().len())
                        .sum::<usize>();
                    let local_baseline = self.stores.generation(session).map_err(unavailable)?;
                    let next = snapshot
                        .encode()
                        .map_err(|reason| error(RefusalCode::NotExportable, reason))?;
                    let session_ref = session.session_ref().map_err(unavailable)?;
                    let offsets = self
                        .door
                        .offsets(&session_ref)
                        .map_err(|failure| door_failure(Op::Prepare, "session.offsets", failure))?;
                    let end = offsets
                        .durable_offset
                        .checked_add(next.len() as u64)
                        .ok_or_else(|| error(RefusalCode::NotExportable, "session_limit"))?;
                    budget
                        .add_stream(end, entries)
                        .map_err(|reason| error(RefusalCode::NotExportable, reason))?;
                    let mut prefix =
                        self.read_local_prefix(&session_ref, offsets.durable_offset, Op::Prepare)?;
                    if prefix.len() as u64 + next.len() as u64 > SESSION_LIMIT {
                        return Err(error(RefusalCode::NotExportable, "session_limit"));
                    }
                    prefix.extend(next);

                    let digest = sha256(&prefix);
                    let file = format!("source-{index}.bin");
                    self.write_cut_file(cut, &file, &prefix)?;
                    if offsets.durable_offset > 0 {
                        let covered = self.door.cover(&session_ref).map_err(|failure| {
                            door_failure(Op::Prepare, "session.cover", failure)
                        })?;
                        if covered.cloud_offset < offsets.durable_offset {
                            return Err(unavailable("previously admitted prefix is not covered"));
                        }
                    }
                    items.push(contract::prepare::PreparedItem::new(
                        &reference.ref_kind,
                        &reference.r#ref,
                        &session_ref,
                        "reserved:aft",
                        prefix.len() as u64,
                        &digest,
                        "aft-session/v1",
                        true,
                    ));
                    streams.push(Stream {
                        session_ref,
                        digest,
                        end: prefix.len() as u64,
                        floor: 0,
                        file,
                        local_baseline,
                        storage_digest: sha256(&prefix),
                    });
                }
                journal.streams = streams;
                journal.refs = Some(refs.clone());
                // Native integration must atomically couple the durable freeze,
                // normalized request and first reply. This saved cut material
                // alone does not close the prepare/freeze crash window.
                self.save(cut, &journal)?;
                barrier.freeze(cut, &sessions).map_err(unavailable)?;
                let reply =
                    serde_json::to_value(contract::prepare::PrepareReply::new(items, Vec::new()))
                        .map_err(unavailable)?;
                self.finish(cut, request, journal, Step::Prepared, reply)
            }
            Op::Export => {
                let mut items = Vec::new();
                for stream in &journal.streams {
                    let bytes = self.read_cut_file(cut, stream)?;
                    let offsets = self
                        .door
                        .offsets(&stream.session_ref)
                        .map_err(|failure| door_failure(Op::Export, "session.offsets", failure))?;
                    if offsets.durable_offset > stream.end {
                        return Err(unavailable("door advanced past prepared cut"));
                    }
                    let admitted_prefix = self.read_local_prefix(
                        &stream.session_ref,
                        offsets.durable_offset,
                        Op::Export,
                    )?;
                    if bytes.get(..admitted_prefix.len()) != Some(admitted_prefix.as_slice()) {
                        return Err(unavailable("door prefix differs from prepared bytes"));
                    }
                    let mut offset = offsets.durable_offset as usize;
                    while offset < bytes.len() {
                        let end = (offset + PAGE_LIMIT).min(bytes.len());
                        let admitted = self
                            .door
                            .admit(&stream.session_ref, offset as u64, &bytes[offset..end])
                            .map_err(|failure| {
                                door_failure(Op::Export, "session.admit", failure)
                            })?;
                        if admitted.durable_offset != end as u64 {
                            return Err(unavailable("invalid admitted offset"));
                        }
                        offset = end;
                    }
                    items.push(json!({"sessionRef":stream.session_ref,"endOffset":stream.end}));
                }
                self.finish(
                    cut,
                    request,
                    journal,
                    Step::Exported,
                    json!({"items":items}),
                )
            }
            Op::Diverged => {
                let mut states = Vec::new();
                for session in &sessions {
                    states.push(json!({"state":if self.local_advanced(session)? { "advanced" } else { "at_cut" }}));
                }
                Ok(json!({"refs":states}))
            }
            Op::Import => {
                let mut streams = Vec::new();
                let mut items = Vec::new();
                let mut total = 0u64;
                let mut budget = super::snapshot::CarryBudget::default();
                for (index, session) in sessions.iter().enumerate() {
                    if self.local_advanced(session)? {
                        return Err(error(RefusalCode::LocalAdvanced, "local session changed"));
                    }
                    let session_ref = session.session_ref().map_err(unavailable)?;
                    let verified = read_verified_cut(
                        self.door.as_ref(),
                        &session_ref,
                        cut,
                        SESSION_LIMIT.min(MOVE_LIMIT.saturating_sub(total)),
                    )
                    .map_err(import_error)?;
                    total = total
                        .checked_add(verified.end)
                        .ok_or_else(|| incompatible("move_limit"))?;
                    if total > MOVE_LIMIT {
                        return Err(incompatible("move_limit"));
                    }
                    let snapshot =
                        SessionSnapshot::decode(&verified.bytes).map_err(incompatible)?;
                    budget
                        .add_stream(
                            verified.end,
                            snapshot
                                .groups
                                .iter()
                                .map(|group| group.files().len())
                                .sum(),
                        )
                        .map_err(incompatible)?;
                    let _ = snapshot
                        .project(self.registry.as_ref(), &mut journal.drops)
                        .map_err(unavailable)?;
                    let local_baseline = self.stores.generation(session).map_err(unavailable)?;
                    let file = format!("staging-{index}.bin");
                    self.write_cut_file(cut, &file, &verified.bytes)?;
                    items.push(contract::import::ImportedItem::new(
                        &session_ref,
                        verified.end,
                        &verified.digest,
                    ));
                    streams.push(Stream {
                        session_ref,
                        digest: verified.digest,
                        end: verified.end,
                        floor: verified.floor,
                        file,
                        local_baseline,
                        storage_digest: sha256(&verified.bytes),
                    });
                }
                journal.streams = streams;
                journal.refs = Some(refs.clone());
                let reply = serde_json::to_value(contract::import::ImportReply::new(items))
                    .map_err(unavailable)?;
                self.finish(cut, request, journal, Step::Imported, reply)
            }
            Op::Activate => {
                let mut projections = Vec::new();
                let mut activation_drops = DropCounts::default();
                for session in &sessions {
                    let session_ref = session.session_ref().map_err(unavailable)?;
                    let stream = journal
                        .streams
                        .iter()
                        .find(|stream| stream.session_ref == session_ref)
                        .ok_or_else(|| unavailable("missing staged session"))?;
                    if self.stores.generation(session).map_err(unavailable)?
                        != stream.local_baseline
                    {
                        return Err(error(
                            RefusalCode::LocalAdvanced,
                            "local session changed after staging",
                        ));
                    }
                    let bytes = self.read_cut_file(cut, stream).map_err(|_| {
                        error(RefusalCode::DigestMismatch, "staging digest mismatch")
                    })?;
                    let snapshot = SessionSnapshot::decode(&bytes).map_err(|_| {
                        error(RefusalCode::DigestMismatch, "staging format changed")
                    })?;
                    projections.push(
                        snapshot
                            .project(self.registry.as_ref(), &mut activation_drops)
                            .map_err(unavailable)?,
                    );
                }
                journal.drops = activation_drops;
                // A daemon host must keep partial publication hidden across a
                // crash before this operation can be exposed. NativeStores
                // writes the two stores separately; it is not transactional.
                barrier.freeze(cut, &sessions).map_err(unavailable)?;
                for (session, projection) in sessions.iter().zip(&projections) {
                    self.stores
                        .publish(session, projection)
                        .map_err(unavailable)?;
                }
                for session in &sessions {
                    self.save_baseline(session)?;
                }
                barrier.activate(cut, &sessions).map_err(unavailable)?;
                self.finish(cut, request, journal, Step::Activated, json!({}))
            }
            Op::Abort => {
                for stream in &journal.streams {
                    self.read_cut_file(cut, stream).map_err(|_| {
                        error(
                            RefusalCode::DigestMismatch,
                            "participant rollback image changed",
                        )
                    })?;
                }
                barrier.abort(cut, &sessions).map_err(unavailable)?;
                self.finish(cut, request, journal, Step::Aborted, json!({}))
            }
            Op::Release => {
                let Request::Release(release) = request else {
                    return Err(unavailable("invalid release request"));
                };
                let enforced = barrier
                    .release(cut, &sessions, &release.holder)
                    .map_err(unavailable)?;
                for session in &sessions {
                    self.save_baseline(session)?;
                }
                self.finish(
                    cut,
                    request,
                    journal,
                    Step::Released,
                    json!({"enforced":enforced}),
                )
            }
            _ => Err(unavailable("unexpected participant operation")),
        }
    }

    fn read_local_prefix(&self, session: &str, end: u64, op: Op) -> Result<Vec<u8>, ErrorBody> {
        if end > SESSION_LIMIT {
            return Err(error(RefusalCode::NotExportable, "session_limit"));
        }
        let mut bytes = Vec::new();
        let mut offset = 0;
        while offset < end {
            let page = self
                .door
                .read(session, offset, PAGE_LIMIT, None)
                .map_err(|failure| door_failure(op, "session.read", failure))?;
            let decoded = super::door::unhex(&page.bytes).map_err(unavailable)?;
            if decoded.is_empty()
                || decoded.len() > PAGE_LIMIT
                || page.from_offset != offset
                || page.end_offset != offset + decoded.len() as u64
                || page.stream_end != end
                || page.end_offset > end
            {
                return Err(unavailable("invalid local door prefix"));
            }
            bytes.extend(decoded);
            offset = page.end_offset;
        }
        Ok(bytes)
    }

    fn load(&self, cut: &str) -> Result<Journal, ErrorBody> {
        let path = self.directory.join(cut).join("journal.json");
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(unavailable),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(error) => Err(unavailable(error)),
        }
    }
    fn save(&self, cut: &str, journal: &Journal) -> Result<(), ErrorBody> {
        let bytes = serde_json::to_vec(journal).map_err(unavailable)?;
        self.write_cut_file(cut, "journal.json", &bytes)
    }
    fn write_cut_file(&self, cut: &str, file: &str, bytes: &[u8]) -> Result<(), ErrorBody> {
        let dir = self.directory.join(cut);
        crate::private_storage::create_dir_all(&dir).map_err(unavailable)?;
        crate::durability::atomic_write(
            &dir.join(file),
            bytes,
            crate::durability::AtomicWriteDurability::FileAndDirectory,
        )
        .map_err(unavailable)
    }
    fn read_cut_file(&self, cut: &str, stream: &Stream) -> Result<Vec<u8>, ErrorBody> {
        if !stream
            .file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
            || stream.file.contains("..")
        {
            return Err(unavailable("invalid staging pointer"));
        }
        let path = self.directory.join(cut).join(&stream.file);
        if std::fs::symlink_metadata(&path).map_err(unavailable)?.len() > SESSION_LIMIT {
            return Err(unavailable("oversize staging file"));
        }
        let bytes = std::fs::read(path).map_err(unavailable)?;
        if sha256(&bytes) != stream.storage_digest {
            return Err(unavailable("participant image digest mismatch"));
        }
        Ok(bytes)
    }
    fn finish(
        &self,
        cut: &str,
        request: &Request,
        mut journal: Journal,
        step: Step,
        reply: Value,
    ) -> Result<Value, ErrorBody> {
        journal.step = step;
        journal.replies.insert(
            request.op().method().to_owned(),
            SavedReply {
                params: request.with_sorted_refs().params(),
                reply: reply.clone(),
            },
        );
        journal.drops.log_internal_drop_report();
        self.save(cut, &journal)?;
        Ok(reply)
    }
    fn baseline_path(&self, session: &ResolvedSession) -> PathBuf {
        let key = sha256(format!("{}\0{}", session.harness, session.session).as_bytes());
        self.directory.join(format!("baseline-{key}.json"))
    }
    fn save_baseline(&self, session: &ResolvedSession) -> Result<(), ErrorBody> {
        let generation = self.stores.generation(session).map_err(unavailable)?;
        crate::durability::atomic_write(
            &self.baseline_path(session),
            generation.as_bytes(),
            crate::durability::AtomicWriteDurability::FileAndDirectory,
        )
        .map_err(unavailable)
    }
    fn local_advanced(&self, session: &ResolvedSession) -> Result<bool, ErrorBody> {
        let current = self.stores.generation(session).map_err(unavailable)?;
        match std::fs::read_to_string(self.baseline_path(session)) {
            Ok(baseline) => Ok(current != baseline),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(current != sha256(&[]))
            }
            Err(error) => Err(unavailable(error)),
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn error(code: RefusalCode, message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(code, message)
}
fn unavailable(reason: impl std::fmt::Display) -> ErrorBody {
    error(RefusalCode::Unavailable, reason.to_string())
}
fn invalid_state(step: Step, refs: Option<&Refs>) -> ErrorBody {
    let mut body = error(RefusalCode::InvalidState, "cut operation is out of order");
    let mut detail = json!({"step":step});
    if let Some(refs) = refs {
        detail["refs"] = json!(refs);
    }
    body.detail = Some(detail);
    body
}
fn incompatible(found: impl std::fmt::Display) -> ErrorBody {
    let mut body = error(
        RefusalCode::IncompatibleVersion,
        "unsupported authenticated AFT snapshot",
    );
    body.detail =
        Some(json!({"found":found.to_string(),"required":"aft-session/v1,64MiB/ref,256MiB/move"}));
    body
}
fn import_error(reason: String) -> ErrorBody {
    match reason.as_str() {
        "cut_not_published" | "not_found" => {
            error(RefusalCode::CutNotReady, "cut is not published")
        }
        "digest_mismatch"
        | "noncontiguous cut"
        | "noncontiguous or changed cut"
        | "nonprogressing cut" => error(RefusalCode::DigestMismatch, reason),
        "session_limit" | "below_floor" => incompatible(reason),
        "verify_failed" => error(RefusalCode::DigestMismatch, reason),
        _ if super::door::classify("session.read", &reason) == super::door::FailureClass::Stop => {
            error(RefusalCode::InvalidRequest, reason)
        }
        _ => unavailable(reason),
    }
}

fn door_failure(op: Op, method: &str, failure: super::door::DoorError) -> ErrorBody {
    if super::door::classify(method, &failure.code) == super::door::FailureClass::Retry {
        return unavailable(failure.code);
    }
    let code = if op == Op::Prepare {
        RefusalCode::NotExportable
    } else {
        RefusalCode::InvalidRequest
    };
    error(
        code,
        format!("session door stopped {method}: {}", failure.code),
    )
}
