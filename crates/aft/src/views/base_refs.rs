//! Durable cross-view base references in the v3 namespace.
//!
//! A base is its own registered v3 member (`kind = base`, no root) under
//! `blobs/v3/<family>/views/<base-scope>`. Its membership is fixed at first
//! publication, and its scope name is derived from its full effective
//! identity. A worker manifest names a base through its `base` header; that
//! header is never trusted. Every consumer resolves the base through
//! [`resolve_base`], which pins the base's current generation with a read
//! marker, re-verifies the pointer and member row, and recomputes both
//! digests from the pinned manifest. Only the resulting
//! [`AuthenticatedBase`] can validate a header or become the base of a
//! composed generation.
//!
//! The worker side of the protocol ([`publish_worker`]) follows the insertion
//! barrier of the views design: a per-acquisition [`BaseRefMarker`] plus an
//! `intent` row in `base_refs` (one IMMEDIATE transaction under the
//! storage-root reservation lock), base re-verification after the barrier,
//! `verify_held` immediately before the worker's pointer CAS, then the row
//! becomes `published`. Aborts delete their own `intent` row under the
//! barrier before the marker drops. A CAS that may have landed is resolved
//! first and never cleaned up as an abort.
//!
//! Nothing here writes a v1 or v2 row, manifest or pointer.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rusqlite::{params, OptionalExtension, Transaction};

use crate::blob_store::v2::to_hex;
use crate::blob_store::v3::{self, LedgerReader, RegistryWriter, ReservationLock};
use crate::root_cache::ReadMarker;

use super::contracts::{boundary, DurabilityObserver, DurabilityStep};
use super::manifest_v2::{
    BaseHeader, EntryV2, GenerationName, ManifestHeader, ManifestV2, PublishV2,
};
use super::registry::STATE_EVICTING;
use super::snapshot::{ComposedGeneration, Residency};
use super::ViewStore;

/// Domain tag of the membership-and-content digest, version 1.
pub const MEMBERSHIP_DIGEST_TAG: &[u8] = b"aft/views-v3/base-membership/1";
/// Domain tag of the effective-identity digest, version 1.
pub const IDENTITY_DIGEST_TAG: &[u8] = b"aft/views-v3/base-identity/1";
/// Membership-keyed base scopes. Tree-keyed scopes use
/// [`TREE_BASE_SCOPE_PREFIX`]; a scope is never relabelled from one to the other.
pub const BASE_SCOPE_PREFIX: &str = "base-m1-";
/// Domain tag of a tree-keyed base's membership digest, version 1. It covers
/// each entry's `git_oid`, which the membership digest refuses.
pub const TREE_MEMBERSHIP_DIGEST_TAG: &[u8] = b"aft/views-v3/base-tree-membership/1";
/// Domain tag of a tree-keyed base's effective-identity digest, version 1.
pub const TREE_IDENTITY_DIGEST_TAG: &[u8] = b"aft/views-v3/base-identity-tree/1";
/// Tree-keyed base scopes. Readers that know only membership-keyed bases
/// refuse this prefix, so they never authenticate such a base.
pub const TREE_BASE_SCOPE_PREFIX: &str = "base-t1-";
/// Pin-then-verify attempts before a base is reported unavailable.
pub const RESOLVE_ATTEMPTS: usize = 3;
/// `base_refs.state` values.
pub const REF_INTENT: &str = "intent";
pub const REF_ORPHANED: &str = "orphaned";
pub const REF_PUBLISHED: &str = "published";
pub const KIND_BASE: &str = "base";
pub const KIND_VIEW: &str = "view";
pub const STATE_ACTIVE: &str = "active";

#[derive(Debug)]
pub enum BaseRefError {
    Store(v3::Error),
    View(super::ViewError),
    Io(io::Error),
    /// The base (or worker) is absent, evicting or moved during every attempt.
    Unavailable(String),
    /// A header, manifest or registry row contradicts the authenticated base.
    Corrupt(String),
}

impl fmt::Display for BaseRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "v3 store: {error}"),
            Self::View(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Unavailable(reason) => write!(f, "base unavailable: {reason}"),
            Self::Corrupt(reason) => write!(f, "corrupt base edge: {reason}"),
        }
    }
}

impl std::error::Error for BaseRefError {}

impl From<v3::Error> for BaseRefError {
    fn from(error: v3::Error) -> Self {
        Self::Store(error)
    }
}
impl From<super::ViewError> for BaseRefError {
    fn from(error: super::ViewError) -> Self {
        Self::View(error)
    }
}
impl From<io::Error> for BaseRefError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<rusqlite::Error> for BaseRefError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(v3::Error::Sqlite(error))
    }
}

pub type Result<T> = std::result::Result<T, BaseRefError>;

/// Length-prefixed, domain-tagged BLAKE3 encoding. Every field carries its
/// length, so no two different field sequences hash the same bytes.
struct TaggedHasher(blake3::Hasher);

impl TaggedHasher {
    fn new(tag: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(tag.len() as u64).to_le_bytes());
        hasher.update(tag);
        Self(hasher)
    }
    fn field(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }
    fn optional(&mut self, value: Option<&[u8]>) {
        match value {
            None => {
                self.0.update(&[0]);
            }
            Some(bytes) => {
                self.0.update(&[1]);
                self.field(bytes);
            }
        }
    }
    fn byte(&mut self, value: u8) {
        self.0.update(&[value]);
    }
    fn u64(&mut self, value: u64) {
        self.0.update(&value.to_le_bytes());
    }
    fn finish(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

/// The membership-and-content digest of a base manifest: every path with its
/// entry kind, mode, content hash, size and resolution-input flag, in path
/// order. Readiness is excluded, so readiness successors keep the digest.
///
/// Membership-keyed bases carry no tree claim: a manifest that claims a HEAD
/// tree or any `git_oid`, or carries composition sections, is not a valid
/// membership-keyed base. Tree claims use [`tree_membership_digest`].
pub fn membership_digest(manifest: &ManifestV2) -> Result<[u8; 32]> {
    if manifest.base_header().is_some() {
        return Err(BaseRefError::Corrupt(
            "a base manifest names another base".into(),
        ));
    }
    if !manifest.tombstones().is_empty() || !manifest.readiness_overrides().is_empty() {
        return Err(BaseRefError::Corrupt(
            "a base manifest carries worker composition sections".into(),
        ));
    }
    if manifest.header().head_tree.is_some() {
        return Err(BaseRefError::Corrupt(
            "a membership-keyed base claims a HEAD tree".into(),
        ));
    }
    let mut hasher = TaggedHasher::new(MEMBERSHIP_DIGEST_TAG);
    hasher.u64(manifest.len() as u64);
    for (path, entry) in manifest.entries() {
        hasher.field(path.as_bytes());
        match entry {
            EntryV2::Regular {
                mode,
                content,
                size,
                git_oid,
                resolution_input,
                ..
            } => {
                if git_oid.is_some() {
                    return Err(BaseRefError::Corrupt(
                        "a membership-keyed base carries an unproven git_oid".into(),
                    ));
                }
                hasher.byte(1);
                hasher.u64(u64::from(*mode));
                hasher.field(content.as_bytes());
                hasher.u64(*size);
                hasher.byte(u8::from(*resolution_input));
            }
            EntryV2::Symlink { target_bytes } => {
                hasher.byte(2);
                hasher.field(target_bytes.as_bytes());
            }
            EntryV2::Gitlink { oid } => {
                hasher.byte(3);
                hasher.field(oid.as_bytes());
            }
            EntryV2::Synthetic { name, .. } => {
                hasher.byte(4);
                hasher.field(name.as_bytes());
            }
        }
    }
    Ok(hasher.finish())
}

/// The effective-identity digest: family, every plane producer, the ignore
/// fingerprint that decided membership, the platform family (path and
/// line-ending conventions), the explicit absence of a tree identity, and
/// the membership digest.
pub fn identity_digest(family: &str, header: &ManifestHeader, membership: &[u8; 32]) -> [u8; 32] {
    let mut hasher = TaggedHasher::new(IDENTITY_DIGEST_TAG);
    hasher.field(family.as_bytes());
    hasher.field(header.producers.trigram.as_bytes());
    hasher.optional(header.producers.semantic.as_deref().map(str::as_bytes));
    hasher.field(header.producers.callgraph.as_bytes());
    hasher.optional(header.ignore_fingerprint.as_deref().map(str::as_bytes));
    hasher.field(std::env::consts::FAMILY.as_bytes());
    hasher.field(b"head_tree:none");
    hasher.field(membership);
    hasher.finish()
}

/// The scope name of the base with this identity.
pub fn base_scope_for(identity: &[u8; 32]) -> String {
    format!("{BASE_SCOPE_PREFIX}{}", to_hex(identity))
}

/// The two kinds of base. The kind follows from the manifest (a `head_tree`
/// makes it tree-keyed) and is visible in the scope prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseKind {
    Membership,
    Tree,
}

/// The kind of base `scope` names, or `None` when it names no base.
pub fn base_kind(scope: &str) -> Option<BaseKind> {
    if scope.starts_with(BASE_SCOPE_PREFIX) {
        Some(BaseKind::Membership)
    } else if scope.starts_with(TREE_BASE_SCOPE_PREFIX) {
        Some(BaseKind::Tree)
    } else {
        None
    }
}

/// The object format of a lowercase-hex object ID, or `None` when `text` is
/// not one. Uppercase is refused so that each OID has a single spelling.
fn object_format_of(text: &str) -> Option<crate::git_index_reader::ObjectFormat> {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then(|| crate::git_index_reader::ObjectFormat::from_hex_len(text.len()))
        .flatten()
}

/// The membership digest of a tree-keyed base: like [`membership_digest`],
/// plus each entry's `git_oid`. Returns the object format shared by the
/// manifest's tree and every OID. These strings are checked for shape only;
/// whether the bytes really are that tree is proven at bind time, never
/// taken from the manifest.
pub fn tree_membership_digest(
    manifest: &ManifestV2,
) -> Result<([u8; 32], crate::git_index_reader::ObjectFormat)> {
    if manifest.base_header().is_some() {
        return Err(BaseRefError::Corrupt(
            "a base manifest names another base".into(),
        ));
    }
    if !manifest.tombstones().is_empty() || !manifest.readiness_overrides().is_empty() {
        return Err(BaseRefError::Corrupt(
            "a base manifest carries worker composition sections".into(),
        ));
    }
    let format = manifest
        .header()
        .head_tree
        .as_deref()
        .and_then(object_format_of)
        .ok_or_else(|| {
            BaseRefError::Corrupt("a tree-keyed base needs a hex HEAD tree object ID".into())
        })?;
    let mut hasher = TaggedHasher::new(TREE_MEMBERSHIP_DIGEST_TAG);
    hasher.u64(manifest.len() as u64);
    for (path, entry) in manifest.entries() {
        let EntryV2::Regular {
            mode,
            content,
            size,
            git_oid: Some(git_oid),
            resolution_input,
            ..
        } = entry
        else {
            return Err(BaseRefError::Corrupt(format!(
                "tree-keyed base entry {path:?} is not a regular file with a git_oid"
            )));
        };
        if object_format_of(git_oid) != Some(format) {
            return Err(BaseRefError::Corrupt(format!(
                "tree-keyed base entry {path:?} has a malformed git_oid"
            )));
        }
        hasher.field(path.as_bytes());
        hasher.byte(1);
        hasher.u64(u64::from(*mode));
        hasher.field(content.as_bytes());
        hasher.u64(*size);
        hasher.byte(u8::from(*resolution_input));
        hasher.field(git_oid.as_bytes());
    }
    Ok((hasher.finish(), format))
}

/// The effective identity of a tree-keyed base: the inputs of
/// [`identity_digest`] with the object format and HEAD tree in place of
/// `head_tree:none`, over the tree-membership digest.
pub fn tree_identity_digest(
    family: &str,
    header: &ManifestHeader,
    format: crate::git_index_reader::ObjectFormat,
    membership: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = TaggedHasher::new(TREE_IDENTITY_DIGEST_TAG);
    hasher.field(family.as_bytes());
    hasher.field(header.producers.trigram.as_bytes());
    hasher.optional(header.producers.semantic.as_deref().map(str::as_bytes));
    hasher.field(header.producers.callgraph.as_bytes());
    hasher.optional(header.ignore_fingerprint.as_deref().map(str::as_bytes));
    hasher.field(std::env::consts::FAMILY.as_bytes());
    hasher.field(format.name().as_bytes());
    let tree = format!("head_tree:{}", header.head_tree.as_deref().unwrap_or(""));
    hasher.field(tree.as_bytes());
    hasher.field(membership);
    hasher.finish()
}

/// The authenticated identity of one base manifest, without any protection.
/// Builders use it before a base is published; readers use
/// [`AuthenticatedBase`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseIdentity {
    pub scope: String,
    pub membership: [u8; 32],
    pub identity: [u8; 32],
}

impl BaseIdentity {
    /// A manifest with a `head_tree` gets a tree-keyed identity; any other
    /// manifest keeps the membership-keyed identity, computed exactly as before.
    pub fn of(family: &str, manifest: &ManifestV2) -> Result<Self> {
        if manifest.header().head_tree.is_some() {
            let (membership, format) = tree_membership_digest(manifest)?;
            let identity = tree_identity_digest(family, manifest.header(), format, &membership);
            return Ok(Self {
                scope: format!("{TREE_BASE_SCOPE_PREFIX}{}", to_hex(&identity)),
                membership,
                identity,
            });
        }
        let membership = membership_digest(manifest)?;
        let identity = identity_digest(family, manifest.header(), &membership);
        Ok(Self {
            scope: base_scope_for(&identity),
            membership,
            identity,
        })
    }

    /// Which kind of base this identity names.
    pub fn kind(&self) -> BaseKind {
        base_kind(&self.scope).expect("identities only derive base scopes")
    }

    pub fn header(&self, bound_generation: &str) -> BaseHeader {
        BaseHeader {
            scope: self.scope.clone(),
            membership_digest: to_hex(&self.membership),
            identity_digest: to_hex(&self.identity),
            bound_generation: bound_generation.to_owned(),
        }
    }

    /// Checks a persisted header against this identity. `bound_generation`
    /// is diagnostic only and never compared.
    pub fn check_header(&self, header: &BaseHeader) -> Result<()> {
        if header.scope != self.scope
            || header.membership_digest != to_hex(&self.membership)
            || header.identity_digest != to_hex(&self.identity)
        {
            return Err(BaseRefError::Corrupt(format!(
                "header for {} does not match the pinned base {}",
                header.scope, self.scope
            )));
        }
        Ok(())
    }
}

/// A base generation protected by an exact-generation read marker and
/// authenticated against its own pinned manifest. Only [`resolve_base`]
/// constructs it.
#[derive(Debug)]
pub struct AuthenticatedBase {
    identity: BaseIdentity,
    generation: String,
    view_dir: PathBuf,
    manifest: ManifestV2,
    marker: ReadMarker,
}

impl AuthenticatedBase {
    pub fn identity(&self) -> &BaseIdentity {
        &self.identity
    }
    pub fn scope(&self) -> &str {
        &self.identity.scope
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn view_dir(&self) -> &Path {
        &self.view_dir
    }
    pub fn manifest(&self) -> &ManifestV2 {
        &self.manifest
    }
    /// The header a worker publishes for this base, bound to this generation.
    pub fn header(&self) -> BaseHeader {
        self.identity.header(&self.generation)
    }
    pub fn check_header(&self, header: &BaseHeader) -> Result<()> {
        self.identity.check_header(header)
    }
    /// The base side of a composed generation. It owns the marker, so the base
    /// generation stays protected for as long as any bundle holds it.
    pub fn into_generation(self, storage: &Path, family: &str) -> Arc<ComposedGeneration> {
        let route = route(storage, family, &self.identity.scope);
        Arc::new(
            ComposedGeneration::new(
                self.generation,
                self.manifest,
                Some(Residency::Marker(self.marker)),
            )
            .with_v3_route(route),
        )
    }
}

pub(crate) fn route(storage: &Path, family: &str, scope: &str) -> super::snapshot::V3Route {
    super::snapshot::V3Route {
        storage: storage.to_path_buf(),
        family: family.to_owned(),
        scope: scope.to_owned(),
    }
}

/// Reads a member row without any lock. `None` means no row.
fn member_row(
    storage: &Path,
    family: &str,
    scope: &str,
) -> Result<Option<(String, String, String)>> {
    let registry = v3::family_dir(storage, family)?.join("members.sqlite");
    if !registry
        .try_exists()
        .map_err(|e| super::io::at("checking base registry", &registry, e))?
    {
        return Ok(None);
    }
    Ok(LedgerReader::open_existing(storage, family)?.member(scope)?)
}

fn expected_dir(storage: &Path, family: &str, scope: &str, stored: &str) -> Result<PathBuf> {
    let dir = v3::view_dir(storage, family, scope)?;
    if Path::new(stored) != dir {
        return Err(BaseRefError::Corrupt(format!(
            "member {scope} names an unexpected view directory"
        )));
    }
    Ok(dir)
}

/// Durable pin-then-verify resolution of `scope`'s current base generation:
/// marker on `readers/<G>/`, then the pointer must still name G and the
/// member must still be a non-evicting `kind = base`, then the manifest's
/// digests are recomputed and must derive `scope`. Retries
/// [`RESOLVE_ATTEMPTS`] times when the pointer or row moves; a missing base is
/// reported unavailable, never replaced by another generation.
pub fn resolve_base(storage: &Path, family: &str, scope: &str) -> Result<AuthenticatedBase> {
    if base_kind(scope).is_none() {
        return Err(BaseRefError::Corrupt(format!(
            "{scope} is not a base scope"
        )));
    }
    for _ in 0..RESOLVE_ATTEMPTS {
        let Some((kind, state, stored_dir)) = member_row(storage, family, scope)? else {
            return Err(BaseRefError::Unavailable(format!(
                "{scope} is not registered"
            )));
        };
        if kind != KIND_BASE {
            return Err(BaseRefError::Corrupt(format!(
                "{scope} is not a base member"
            )));
        }
        if state == STATE_EVICTING {
            return Err(BaseRefError::Unavailable(format!("{scope} is evicting")));
        }
        let view_dir = expected_dir(storage, family, scope, &stored_dir)?;
        let Some(store) = ViewStore::existing_dir(view_dir.clone()) else {
            return Err(BaseRefError::Unavailable(format!("{scope} has no pointer")));
        };
        let Some(generation) = store.current_generation_read_only()? else {
            return Err(BaseRefError::Unavailable(format!("{scope} is unpublished")));
        };
        let marker = ReadMarker::create(&view_dir, &generation)?;
        if store.current_generation_read_only()?.as_deref() != Some(generation.as_str()) {
            continue;
        }
        match member_row(storage, family, scope)? {
            Some((kind, state, _)) if kind == KIND_BASE && state != STATE_EVICTING => {}
            _ => continue,
        }
        let manifest = store.load_manifest_v2(&generation)?;
        let identity = BaseIdentity::of(family, &manifest)?;
        if identity.scope != scope {
            return Err(BaseRefError::Corrupt(format!(
                "{scope} holds a manifest of identity {}",
                identity.scope
            )));
        }
        return Ok(AuthenticatedBase {
            identity,
            generation,
            view_dir,
            manifest,
            marker,
        });
    }
    Err(BaseRefError::Unavailable(format!(
        "{scope} moved during {RESOLVE_ATTEMPTS} attempts"
    )))
}

static NONCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A process-unique, unguessable-enough acquisition identity.
pub(crate) fn fresh_nonce() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hasher.update(&NONCE_SEQ.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    to_hex(&hasher.finalize().as_bytes()[..16])
}

/// The insertion-barrier marker: a read marker in the base's `readers/<G>/`
/// whose file records a per-acquisition nonce. Its path is unique to this
/// acquisition, so another holder's release never removes it.
#[derive(Debug)]
pub struct BaseRefMarker {
    marker: ReadMarker,
    nonce: String,
    generation: String,
}

impl BaseRefMarker {
    fn acquire(view_dir: &Path, generation: &str) -> io::Result<Self> {
        let nonce = fresh_nonce();
        Ok(Self {
            marker: ReadMarker::create_with_nonce(view_dir, generation, &nonce)?,
            nonce,
            generation: generation.to_owned(),
        })
    }
    /// True while the marker file at its own path still carries this
    /// acquisition's nonce.
    pub fn verify_held(&self) -> io::Result<bool> {
        self.marker.verify_held()
    }
    pub fn touch_if_due(&self) -> io::Result<()> {
        self.marker.touch_if_due()
    }
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn path(&self) -> &Path {
        self.marker.path()
    }
}

/// True when the registry has the base-reference tables.
pub(crate) fn has_base_refs(tx: &rusqlite::Connection) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='base_refs')",
        [],
        |r| r.get(0),
    )
}

pub(crate) fn gc_epoch(tx: &rusqlite::Connection) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT gc_epoch FROM sweep_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )
}

/// The base barrier: the storage-root reservation lock, then one IMMEDIATE
/// transaction on the family registry, upgraded to the base-reference
/// schema. A base has no view session, so this is its equivalent of
/// `ViewRegistration::under_pin_barrier`. Deregistration and reclaim take
/// the same barrier.
pub fn with_base_barrier<T>(
    storage: &Path,
    family: &str,
    body: impl FnOnce(&Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let lock = ReservationLock::acquire(storage)?;
    let mut registry = v3::open_base_refs_registry(storage, family, &lock, None)?;
    barrier(&mut registry, body)
}

/// One barrier transaction on an already-open registry. An error from `body`
/// rolls the transaction back.
pub(crate) fn barrier<T>(
    registry: &mut RegistryWriter<'_>,
    body: impl FnOnce(&Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let mut outcome = None;
    let result = registry.with_barrier(|tx| match body(tx) {
        Ok(value) => {
            outcome = Some(Ok(value));
            Ok(())
        }
        Err(error) => {
            outcome = Some(Err(error));
            // Any refusal rolls back: nothing the closure wrote survives.
            Err(v3::Error::Refused("base_barrier_body_failed"))
        }
    });
    match (result, outcome) {
        (_, Some(Err(error))) => Err(error),
        (Err(error), _) => Err(error.into()),
        (Ok(()), Some(value)) => value,
        (Ok(()), None) => unreachable!("the barrier ran its body"),
    }
}

/// Registers (or re-binds) a v3 worker view member inside a barrier. A member
/// in `evicting` is never reused: its directory is about to disappear.
pub(crate) fn register_view_in(
    tx: &Transaction<'_>,
    storage: &Path,
    family: &str,
    scope: &str,
    root: &Path,
) -> Result<PathBuf> {
    if base_kind(scope).is_some() {
        return Err(BaseRefError::Corrupt(format!(
            "{scope} is reserved for bases"
        )));
    }
    let dir = v3::view_dir(storage, family, scope)?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,state FROM members WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((_, state)) if state == STATE_EVICTING => {
            return Err(BaseRefError::Unavailable(format!(
                "worker member {scope} is evicting"
            )));
        }
        Some((kind, _)) if kind != KIND_VIEW => {
            return Err(BaseRefError::Corrupt(format!(
                "{scope} is not a view member"
            )));
        }
        _ => {}
    }
    tx.execute(
        "INSERT INTO members(scope,kind,root,view_dir,registered_at_ms,last_bind_ms,state) VALUES(?1,'view',?2,?3,?4,?4,'active') ON CONFLICT(scope) DO UPDATE SET last_bind_ms=excluded.last_bind_ms,root=excluded.root,missing_sweeps=0",
        params![scope, encode_root(root), dir.to_string_lossy(), crate::pins::now_ms() as i64],
    )?;
    Ok(dir)
}

fn encode_root(root: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(root.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        root.to_str().map(|text| text.as_bytes().to_vec())
    }
}

/// Deletes one acquisition's `intent` row. Never touches a `published` row:
/// a landed reference is not abort cleanup.
pub(crate) fn delete_intent(
    tx: &Transaction<'_>,
    worker_scope: &str,
    base_scope: &str,
    nonce: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM base_refs WHERE worker_scope=?1 AND base_scope=?2 AND nonce=?3 AND state='intent'",
        params![worker_scope, base_scope, nonce],
    )
}

/// Turns one acquisition's row into the durable reference of
/// `worker_generation`, re-inserting it if a sweep already retired it.
pub(crate) fn publish_referrer(
    tx: &Transaction<'_>,
    worker_scope: &str,
    base_scope: &str,
    nonce: &str,
    membership_digest: &str,
    worker_generation: &str,
) -> rusqlite::Result<()> {
    let epoch = gc_epoch(tx)?;
    let owner = super::registry::current_owner();
    tx.execute(
        "INSERT INTO base_refs(worker_scope,base_scope,nonce,membership_digest,worker_generation,state,owner_pid,owner_start,updated_epoch) VALUES(?1,?2,?3,?4,?5,'published',?6,?7,?8) ON CONFLICT(worker_scope,base_scope,nonce) DO UPDATE SET state='published',worker_generation=excluded.worker_generation,membership_digest=excluded.membership_digest,updated_epoch=excluded.updated_epoch",
        params![
            worker_scope,
            base_scope,
            nonce,
            membership_digest,
            worker_generation,
            owner.pid,
            owner.start_time as i64,
            epoch
        ],
    )?;
    // A republication supersedes this worker's strict-rebuild record.
    tx.execute(
        "DELETE FROM strict_rebuild WHERE worker_scope=?1",
        [worker_scope],
    )?;
    Ok(())
}

/// One worker publication that references a base.
pub struct WorkerPublication<'a> {
    pub storage: &'a Path,
    pub family: &'a str,
    pub worker_scope: &'a str,
    pub root: &'a Path,
    /// The base header the delta was derived from. Its scope and digests are
    /// checked against the pinned base; its generation is diagnostic.
    pub base: &'a BaseHeader,
    /// The delta: entries, tombstones and readiness overrides. Its `base`
    /// header is replaced with the authenticated one before it is written.
    pub delta: ManifestV2,
    /// The durable worker generation the pointer CAS expects (`Wold`).
    pub previous: Option<String>,
}

/// What a worker publication did.
#[derive(Debug)]
pub enum WorkerOutcome {
    /// The pointer names the new generation and its referrer row is
    /// `published`. The composed generation owns exact-generation markers on
    /// both the worker and the base generation.
    Published {
        generation: String,
        composed: Arc<ComposedGeneration>,
    },
    /// Another publisher won the CAS. This job's intent row is gone; the caller
    /// schedules a fresh, revalidated job against the winner.
    Lost {
        current: Option<String>,
        equivalent: bool,
    },
}

// Test seam: runs after the worker's pointer CAS was attempted and before
// its outcome is handled, so tests can fail or observe that window.
#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_WORKER_CAS: std::cell::RefCell<Option<Box<dyn FnMut() -> Option<io::Error>>>> =
        const { std::cell::RefCell::new(None) };
}

/// Publishes a worker generation that references a base, following the
/// insertion barrier and publication order of the views design.
pub fn publish_worker(
    request: WorkerPublication<'_>,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<WorkerOutcome> {
    publish_worker_with(request, observer, &mut |_| Ok(()), &mut || Ok(()))
}

/// [`publish_worker`] with a step that runs once the final generation name is
/// known (after the delta is rebound to the authenticated base header and
/// its manifest is durable) and before the pointer CAS. Plane finishers
/// rename private derived files to that name here. An error aborts the job
/// like a failed verify.
/// `final_validate` runs after marker and registry verification, immediately
/// before the pointer CAS, so admission and artifact work cannot stale a
/// checkout's freshness proof.
pub fn publish_worker_with(
    request: WorkerPublication<'_>,
    observer: Option<&dyn DurabilityObserver>,
    before_cas: &mut dyn FnMut(&str) -> Result<()>,
    final_validate: &mut dyn FnMut() -> Result<()>,
) -> Result<WorkerOutcome> {
    publish_worker_guarded(
        request,
        observer,
        before_cas,
        final_validate,
        &mut |commit| commit(),
    )
}

/// Publishes with ownership serialization around final validation and the CAS.
/// `publish_cas` may decline without calling its operation, but may call it only
/// once. Artifact construction and admission happen before this fence.
pub fn publish_worker_guarded(
    request: WorkerPublication<'_>,
    observer: Option<&dyn DurabilityObserver>,
    before_cas: &mut dyn FnMut(&str) -> Result<()>,
    final_validate: &mut dyn FnMut() -> Result<()>,
    publish_cas: &mut dyn FnMut(&mut dyn FnMut() -> Result<PublishV2>) -> Result<PublishV2>,
) -> Result<WorkerOutcome> {
    let WorkerPublication {
        storage,
        family,
        worker_scope,
        root,
        base,
        mut delta,
        previous,
    } = request;
    let base_scope = base.scope.as_str();
    // (1) Inside the barrier: confirm the base, create the marker, then the
    // intent row. File first: a rollback leaves only an orphan marker, which
    // a sweep reclaims once this process is gone.
    let (worker_dir, marker) = with_base_barrier(storage, family, |tx| {
        let worker_dir = register_view_in(tx, storage, family, worker_scope, root)?;
        let (kind, state, stored): (String, String, String) = tx
            .query_row(
                "SELECT kind,state,view_dir FROM members WHERE scope=?1",
                [base_scope],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| BaseRefError::Unavailable(format!("{base_scope} is not registered")))?;
        if kind != KIND_BASE {
            return Err(BaseRefError::Corrupt(format!("{base_scope} is not a base")));
        }
        if state == STATE_EVICTING {
            return Err(BaseRefError::Unavailable(format!(
                "{base_scope} is evicting"
            )));
        }
        let base_dir = expected_dir(storage, family, base_scope, &stored)?;
        let generation = ViewStore::existing_dir(base_dir.clone())
            .map(|store| store.current_generation_read_only())
            .transpose()?
            .flatten()
            .ok_or_else(|| BaseRefError::Unavailable(format!("{base_scope} has no pointer")))?;
        let marker = BaseRefMarker::acquire(&base_dir, &generation)?;
        let owner = super::registry::current_owner();
        tx.execute(
            "INSERT INTO base_refs(worker_scope,base_scope,nonce,membership_digest,worker_generation,state,owner_pid,owner_start,updated_epoch) VALUES(?1,?2,?3,?4,NULL,'intent',?5,?6,?7)",
            params![
                worker_scope,
                base_scope,
                marker.nonce(),
                base.membership_digest,
                owner.pid,
                owner.start_time as i64,
                gc_epoch(tx)?
            ],
        )?;
        Ok((worker_dir, marker))
    })?;
    let marked = boundary(observer, DurabilityStep::BaseRefMarked);
    // Terminal cleanup: this acquisition's row goes under the barrier before
    // its marker drops.
    let cleanup = |marker: BaseRefMarker| -> Result<()> {
        let result = with_base_barrier(storage, family, |tx| {
            delete_intent(tx, worker_scope, base_scope, marker.nonce())?;
            Ok(())
        });
        drop(marker);
        result
    };
    let abort = |error: BaseRefError, marker: BaseRefMarker| -> BaseRefError {
        match cleanup(marker) {
            Ok(()) => error,
            Err(cleanup) => {
                BaseRefError::Unavailable(format!("{error}; intent cleanup failed: {cleanup}"))
            }
        }
    };

    if let Err(error) = marked {
        return Err(abort(error.into(), marker));
    }

    // (2) After the barrier: authenticate the base (pin-then-verify) and
    // check that it is the marked generation or a readiness successor with
    // the expected membership.
    let authenticated = match resolve_base(storage, family, base_scope)
        .and_then(|resolved| resolved.check_header(base).map(|()| resolved))
    {
        Ok(resolved) => resolved,
        Err(error) => return Err(abort(error, marker)),
    };
    let header = authenticated.header();
    if let Err(error) = delta.set_base_header(header.clone()) {
        return Err(abort(error.into(), marker));
    }
    let base_generation = authenticated.into_generation(storage, family);
    let worker_store = match ViewStore::open_dir(worker_dir.clone()) {
        Ok(store) => store,
        Err(error) => return Err(abort(error.into(), marker)),
    };
    let name = match GenerationName::for_manifest(&delta) {
        Ok(name) => name,
        Err(error) => return Err(abort(error.into(), marker)),
    };
    let generation = name.to_string();
    let prepared = match worker_store.prepare_v2(&name, previous.as_deref(), &delta, observer) {
        Ok(prepared) => prepared,
        Err(error) => return Err(abort(error.into(), marker)),
    };
    if let Err(error) = before_cas(&generation) {
        return Err(abort(error, marker));
    }
    // The bundle's own marker on the new worker generation exists before the
    // CAS, so the generation is protected from the moment it is published.
    let worker_marker = match ReadMarker::create(&worker_dir, &generation) {
        Ok(marker) => marker,
        Err(error) => return Err(abort(error.into(), marker)),
    };
    let composed = match ComposedGeneration::compose(
        generation.clone(),
        delta,
        base_generation,
        Some(Residency::Marker(worker_marker)),
    ) {
        Ok(composed) => Arc::new(composed.with_v3_route(route(storage, family, worker_scope))),
        Err(error) => return Err(abort(error.into(), marker)),
    };

    // (3) Immediately before the CAS: the acquisition marker is still ours,
    // and the base is still a registered, non-evicting base.
    let verified = marker
        .verify_held()
        .map_err(BaseRefError::from)
        .and_then(|held| {
            if !held {
                return Err(BaseRefError::Unavailable(
                    "the base marker was reclaimed before publication".into(),
                ));
            }
            with_base_barrier(storage, family, |tx| {
                let state: Option<(String, String)> = tx
                    .query_row(
                        "SELECT kind,state FROM members WHERE scope=?1",
                        [base_scope],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match state {
                    Some((kind, state)) if kind == KIND_BASE && state != STATE_EVICTING => Ok(()),
                    _ => Err(BaseRefError::Unavailable(format!(
                        "{base_scope} left the registry before publication"
                    ))),
                }
            })
        });
    if let Err(error) = verified {
        return Err(abort(error, marker));
    }

    // (4) The CAS. From here on the reference may be durable.
    let mut prepared = Some(prepared);
    let mut invoked = false;
    let mut cas_started = false;
    let outcome = publish_cas(&mut || {
        if invoked {
            return Err(BaseRefError::Unavailable(
                "publication fence invoked CAS twice".into(),
            ));
        }
        invoked = true;
        final_validate()?;
        cas_started = true;
        worker_store
            .commit_v2(
                prepared.take().expect("prepared CAS consumed once"),
                observer,
            )
            .map_err(BaseRefError::from)
    });
    if !cas_started {
        return Err(abort(
            outcome.err().unwrap_or_else(|| {
                BaseRefError::Unavailable("publication fence did not invoke CAS".into())
            }),
            marker,
        ));
    }
    #[cfg(test)]
    let outcome =
        match AFTER_WORKER_CAS.with(|hook| hook.borrow_mut().as_mut().and_then(|hook| hook())) {
            Some(error) => Err(error.into()),
            None => outcome,
        };
    let landed = match outcome {
        Ok(PublishV2::Published) => Ok(()),
        Ok(PublishV2::Conflict { current }) => {
            cleanup(marker)?;
            return Ok(WorkerOutcome::Lost {
                current,
                equivalent: false,
            });
        }
        Ok(PublishV2::EquivalentWinner { current }) => {
            cleanup(marker)?;
            return Ok(WorkerOutcome::Lost {
                current: Some(current),
                equivalent: true,
            });
        }
        Err(error) => {
            // The CAS may have landed. Resolve it while keeping protection: a
            // landed reference becomes `published`; only a pointer that
            // certainly names something else allows abort cleanup; an
            // unreadable pointer keeps the intent row for reconciliation.
            match worker_store.current_generation_read_only() {
                Ok(Some(current)) if current == generation => Err(error),
                Ok(_) => return Err(abort(error.into(), marker)),
                Err(_) => {
                    drop(marker);
                    return Err(error.into());
                }
            }
        }
    };
    // (5) ReferrerPublished, under the barrier, before the marker drops.
    let published = with_base_barrier(storage, family, |tx| {
        publish_referrer(
            tx,
            worker_scope,
            base_scope,
            marker.nonce(),
            &header.membership_digest,
            &generation,
        )?;
        Ok(())
    });
    match published.and_then(|()| Ok(boundary(observer, DurabilityStep::ReferrerPublished)?)) {
        Ok(()) => {}
        Err(error) => {
            // The reference stays protected: either the row is already
            // `published`, or it is still `intent` and reconciliation repairs
            // it from the published edge after this owner is gone.
            drop(marker);
            return Err(error);
        }
    }
    // (6) The bundle owns its exact-generation markers; the insertion marker
    // can go.
    drop(marker);
    landed.map_err(BaseRefError::from)?;
    Ok(WorkerOutcome::Published {
        generation,
        composed,
    })
}

/// Opens a v3 worker's published generation for a foreign or parent-folder
/// reader with a dual pin-then-verify: the worker's current W, then, when W
/// names a base, the base's current generation through [`resolve_base`],
/// checked against W's header. On any failed verify both markers drop and the
/// whole open retries, at most [`RESOLVE_ATTEMPTS`] times; then the base is
/// reported unavailable. The returned bundle owns both markers. Its answers are
/// those of the worker's last publication, labelled by
/// [`ComposedGeneration::published`].
pub fn open_foreign_v3(
    storage: &Path,
    family: &str,
    scope: &str,
    producers: &super::manifest_v2::Producers,
) -> Result<Option<Arc<ComposedGeneration>>> {
    let mut last = None;
    for _ in 0..RESOLVE_ATTEMPTS {
        let Some((kind, state, stored)) = member_row(storage, family, scope)? else {
            return Ok(None);
        };
        if kind != KIND_VIEW {
            return Err(BaseRefError::Corrupt(format!(
                "{scope} is not a view member"
            )));
        }
        if state == STATE_EVICTING {
            return Ok(None);
        }
        let view_dir = expected_dir(storage, family, scope, &stored)?;
        let Some(store) = ViewStore::existing_dir(view_dir.clone()) else {
            return Ok(None);
        };
        let Some(generation) = store.current_generation_read_only()? else {
            return Ok(None);
        };
        let marker = ReadMarker::create(&view_dir, &generation)?;
        if store.current_generation_read_only()?.as_deref() != Some(generation.as_str()) {
            continue;
        }
        let manifest = store.load_manifest_v2(&generation)?;
        manifest.ensure_producers(producers)?;
        let Some(header) = manifest.base_header().cloned() else {
            return Ok(Some(Arc::new(
                ComposedGeneration::new(generation, manifest, Some(Residency::Marker(marker)))
                    .with_v3_route(route(storage, family, scope)),
            )));
        };
        let base = match resolve_base(storage, family, &header.scope)
            .and_then(|base| base.check_header(&header).map(|()| base))
        {
            Ok(base) => base,
            Err(error @ BaseRefError::Unavailable(_)) => {
                last = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        return Ok(Some(Arc::new(
            ComposedGeneration::compose(
                generation,
                manifest,
                base.into_generation(storage, family),
                Some(Residency::Marker(marker)),
            )?
            .with_v3_route(route(storage, family, scope)),
        )));
    }
    Err(last.unwrap_or_else(|| {
        BaseRefError::Unavailable(format!("{scope} moved during {RESOLVE_ATTEMPTS} attempts"))
    }))
}

#[cfg(test)]
#[path = "base_refs_tests.rs"]
pub(crate) mod tests;
