//! The two chunk queries Occipital uses to fetch rich-UI assets by digest.
//!
//! `rich_ui.manifest_chunk {manifestDigest, index}` serves the running build's
//! canonical manifest bytes and `rich_ui.template_chunk {templateDigest,
//! index}` serves template bytes, either shipped in the binary or stored in
//! aft.db by a view record. Both are provider reads: they take only a digest
//! and a 0-based chunk index, never a session, and answer
//! `{digest, index, chunks, bytes, data}` where `data` is standard padded
//! base64 of at most [`CHUNK_BYTES`] decoded bytes, `chunks` is the total
//! chunk count and `bytes` is the whole asset's length.
//!
//! Only the authenticated principal `reserved:occipital` may call them. Every
//! refusal is an [`ErrorBody`] carrying one of the refusal codes listed in
//! `tests/fixtures/rich_ui/transport-contract.json` (`chunks.refusalCodes`),
//! with the fixed message from [`operations::fixed_message`], so no request
//! data ever appears in a refusal.

use std::ops::Deref;
use std::sync::{Mutex, TryLockError};

use base64::Engine as _;
use serde_json::{json, Value};
use subc_protocol::ErrorBody;

use super::builder::CHUNK_BYTES;
use super::operations;

pub(crate) const MANIFEST_CHUNK: &str = "rich_ui.manifest_chunk";
pub(crate) const TEMPLATE_CHUNK: &str = "rich_ui.template_chunk";

/// The only principal admitted to the chunk queries (and the rich-UI carrier).
pub(crate) const OCCIPITAL_PRINCIPAL: &str = "reserved:occipital";

pub(crate) fn is_chunk_operation(operation: &str) -> bool {
    matches!(operation, MANIFEST_CHUNK | TEMPLATE_CHUNK)
}

/// A refusal with its code and the code's fixed, request-free message.
pub(crate) fn refusal(code: &str) -> ErrorBody {
    ErrorBody::new(code, operations::fixed_message(code))
}

/// Refuse every principal other than Occipital, before any lookup runs.
pub(crate) fn admit_principal(principal_id: Option<&str>) -> Result<(), ErrorBody> {
    if principal_id == Some(OCCIPITAL_PRINCIPAL) {
        Ok(())
    } else {
        Err(refusal("principal_not_allowed"))
    }
}

/// The manifest bytes the running build publishes, with their digest. The
/// production builder does not publish a manifest yet, so this is `None`
/// outside tests; tests inject the golden manifest through
/// [`set_test_manifest_for_test`].
pub(super) fn running_manifest() -> Option<(String, Vec<u8>)> {
    #[cfg(test)]
    {
        if let Some(manifest) = TEST_MANIFEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Some(manifest);
        }
    }
    super::builder::production_manifest().map(|manifest| (manifest.digest, manifest.bytes))
}

#[cfg(test)]
static TEST_MANIFEST: Mutex<Option<(String, Vec<u8>)>> = Mutex::new(None);

/// Serializes tests that inject a manifest, since the injection is
/// process-global; the guard clears the injection when dropped.
#[cfg(test)]
static TEST_MANIFEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) struct TestManifestGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestManifestGuard {
    fn drop(&mut self) {
        *TEST_MANIFEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Hold the returned guard while the injected manifest must stay in place.
/// `None` asserts the production state: no manifest at all.
#[cfg(test)]
pub(crate) fn set_test_manifest_for_test(bytes: Option<Vec<u8>>) -> TestManifestGuard {
    let serial = TEST_MANIFEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *TEST_MANIFEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        bytes.map(|bytes| (super::builder::digest(&bytes), bytes));
    TestManifestGuard { _serial: serial }
}

/// Template bytes embedded in the binary, by digest. The binary embeds no
/// template yet, so this lookup always returns `None` and every template is
/// served from aft.db.
fn shipped_template(_digest: &str) -> Option<&'static [u8]> {
    None
}

/// The request's digest and index. A digest that is missing or not a string
/// names no asset (`digest_unknown`); an index that is missing or not a
/// non-negative integer names no chunk (`chunk_index_out_of_range`, checked
/// once the asset is found). A malformed request therefore gets one of the
/// same refusal codes as a well-formed one, rather than a separate code.
fn request(body: &Value, digest_field: &str) -> Result<(String, Option<u64>), ErrorBody> {
    let digest = body
        .get(digest_field)
        .and_then(Value::as_str)
        .ok_or_else(|| refusal("digest_unknown"))?;
    Ok((digest.to_owned(), body.get("index").and_then(Value::as_u64)))
}

/// Build the reply for chunk `index` of `bytes`, whose digest is `digest`.
pub(crate) fn chunk_reply(
    digest: &str,
    bytes: &[u8],
    index: Option<u64>,
) -> Result<Value, ErrorBody> {
    let chunks = bytes.len().div_ceil(CHUNK_BYTES);
    let index = index
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < chunks)
        .ok_or_else(|| refusal("chunk_index_out_of_range"))?;
    let start = index * CHUNK_BYTES;
    let end = (start + CHUNK_BYTES).min(bytes.len());
    Ok(json!({
        "digest": digest,
        "index": index,
        "chunks": chunks,
        "bytes": bytes.len(),
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]),
    }))
}

/// Answer `rich_ui.manifest_chunk`. It compares only against the running
/// build's manifest and never opens the database: with no manifest, or for
/// any other digest, it refuses `digest_unknown`.
pub(crate) fn manifest_chunk(body: &Value) -> Result<Value, ErrorBody> {
    let (digest, index) = request(body, "manifestDigest")?;
    let (manifest_digest, bytes) = running_manifest()
        .filter(|(manifest_digest, _)| *manifest_digest == digest)
        .ok_or_else(|| refusal("digest_unknown"))?;
    chunk_reply(&manifest_digest, &bytes, index)
}

/// Answer `rich_ui.template_chunk`. The binary's shipped templates are
/// checked first and need no database. Otherwise the stored templates in the
/// process's one aft.db are read without waiting for its lock: no handle or a
/// poisoned mutex refuses `database_unavailable`, a held mutex refuses
/// `database_busy`, and only a lookup that ran and found nothing refuses
/// `digest_unknown`. A failed query is also `database_unavailable`, because
/// an unreadable database must never be reported as a missing digest.
pub(crate) fn template_chunk<C>(body: &Value, db: Option<&Mutex<C>>) -> Result<Value, ErrorBody>
where
    C: Deref<Target = rusqlite::Connection>,
{
    let (digest, index) = request(body, "templateDigest")?;
    if let Some(bytes) = shipped_template(&digest) {
        return chunk_reply(&digest, bytes, index);
    }
    let db = db.ok_or_else(|| refusal("database_unavailable"))?;
    let conn = match db.try_lock() {
        Ok(conn) => conn,
        Err(TryLockError::WouldBlock) => return Err(refusal("database_busy")),
        Err(TryLockError::Poisoned(_)) => return Err(refusal("database_unavailable")),
    };
    let stored = crate::db::rich_ui::template_bytes(&conn, &digest)
        .map_err(|_| refusal("database_unavailable"))?;
    drop(conn);
    let bytes = stored.ok_or_else(|| refusal("digest_unknown"))?;
    chunk_reply(&digest, &bytes, index)
}

#[cfg(test)]
#[path = "chunks/tests.rs"]
pub(crate) mod tests;
