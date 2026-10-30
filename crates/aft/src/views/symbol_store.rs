//! The v3 symbol plane: parsed symbols of one file, shared by every checkout
//! of a family and keyed by what decides them, never by path.
//!
//! Storage is the ordinary v3 plane store `blobs/v3/<family>/symbol.sqlite`
//! (`FamilyPlane::Symbol`, format in docs/design/views-v3-format.md, *Import
//! ledger and symbol plane*). This module owns the plane's key, payload,
//! residency and the policy parts of its GC and retention:
//!
//! - **Key.** Content hash, language and producer (the extraction schema,
//!   `symbol_cache_disk::SCHEMA_VERSION`). A parser or schema change changes
//!   the producer, so its lookups miss instead of returning stale symbols.
//! - **Writes** go through `blob_store::v3::publish` only: the v3 reader floor
//!   is raised first and every batch holds its own reservation.
//! - **GC.** No manifest names a symbol row, so the v3 plane-row sweep keeps
//!   a row until it has gone [`SYMBOL_RETENTION_SWEEPS`] sweep epochs without
//!   a put or [`touch`]. Callers that rely on a row touch it.
//! - **Retention.** Rows count in the family's v3 namespace inventory and
//!   budget like every plane; nothing else deletes them.
//! - **Residency.** Decoded symbol lists are shared as `Arc<Vec<Symbol>>`
//!   through one process-wide cache bounded by [`RESIDENT_BYTES`]; evicting
//!   from it never touches stored rows.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane};
use crate::blob_store::v3;
use crate::symbols::Symbol;

pub use crate::blob_store::v3::SYMBOL_RETENTION_SWEEPS;

/// Layout byte of a symbol payload.
const PAYLOAD_LAYOUT: u8 = 1;
const KEY_TAG: &[u8] = b"aft/views-v3/symbol/v1";
/// Estimated bytes of decoded symbol lists kept resident per process.
pub const RESIDENT_BYTES: u64 = 64 << 20;
/// Payload bytes written by one v3 publication.
const PUT_BATCH_BYTES: u64 = 32 << 20;

/// What produced a symbol list: the extraction schema, which is bumped on
/// any grammar, query or extractor change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SymbolProducer {
    pub extraction_schema: u32,
}

impl SymbolProducer {
    /// This build's producer.
    pub fn current() -> Self {
        Self {
            extraction_schema: crate::symbol_cache_disk::SCHEMA_VERSION,
        }
    }

    pub fn id(&self) -> String {
        format!("aft-symbols/extraction-{}", self.extraction_schema)
    }
}

/// The language id stored in symbol keys and payloads: the parser's
/// language name.
pub fn language_id(language: crate::parser::LangId) -> String {
    format!("{language:?}")
}

/// The language a path's symbols are extracted with, if any.
pub fn language_of(path: &Path) -> Option<String> {
    crate::parser::detect_language(path).map(language_id)
}

/// Everything that decides a symbol list.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SymbolKey {
    pub content: ContentHash,
    pub language: String,
    pub producer: SymbolProducer,
}

impl SymbolKey {
    pub fn family_key(&self) -> FamilyKey {
        let mut hasher = blake3::Hasher::new();
        hasher.update(KEY_TAG);
        for field in [
            &self.content.as_bytes()[..],
            self.language.as_bytes(),
            self.producer.id().as_bytes(),
        ] {
            hasher.update(&(field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        FamilyKey::new(FamilyPlane::Symbol, *hasher.finalize().as_bytes())
    }
}

fn push_field(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

/// The canonical payload of `symbols`: the layout byte, then the producer
/// id, the language id and the symbols as JSON, each length-prefixed.
pub fn encode_payload(producer: &SymbolProducer, language: &str, symbols: &[Symbol]) -> Vec<u8> {
    let json = serde_json::to_vec(symbols).expect("symbols serialize");
    let mut payload = vec![PAYLOAD_LAYOUT];
    push_field(&mut payload, producer.id().as_bytes());
    push_field(&mut payload, language.as_bytes());
    push_field(&mut payload, &json);
    payload
}

/// Decodes a payload stored for `producer` and `language`; a payload of any
/// other layout, producer or language is refused, never reinterpreted.
pub fn decode_payload(
    payload: &[u8],
    producer: &SymbolProducer,
    language: &str,
) -> Result<Vec<Symbol>, String> {
    let mut rest = payload
        .strip_prefix(&[PAYLOAD_LAYOUT])
        .ok_or("symbol payload has another layout")?;
    let mut field = || -> Result<&[u8], String> {
        if rest.len() < 4 {
            return Err("symbol payload is truncated".to_owned());
        }
        let len = u32::from_le_bytes(rest[..4].try_into().expect("4")) as usize;
        let value = rest.get(4..4 + len).ok_or("symbol payload is truncated")?;
        rest = &rest[4 + len..];
        Ok(value)
    };
    if field()? != producer.id().as_bytes() {
        return Err("symbol payload has another producer".to_owned());
    }
    if field()? != language.as_bytes() {
        return Err("symbol payload has another language".to_owned());
    }
    let json = field()?;
    if !rest.is_empty() {
        return Err("symbol payload has trailing bytes".to_owned());
    }
    serde_json::from_slice(json).map_err(|error| format!("symbol payload JSON: {error}"))
}

/// Stores `(key, payload)` rows, in batches of at most 32 MiB of payload,
/// each one v3 publication under its own reservation. Returns how many rows
/// were inserted (the rest were equal rows, only touched).
pub fn put(storage: &Path, family: &str, rows: &[(FamilyKey, Vec<u8>)]) -> Result<u64, v3::Error> {
    let schema = FamilyPlane::Symbol.payload_schema();
    let mut inserted = 0;
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        let mut bytes = 0_u64;
        while end < rows.len() && (end == start || bytes < PUT_BATCH_BYTES) {
            bytes += rows[end].1.len() as u64;
            end += 1;
        }
        let request = bytes.saturating_mul(2).saturating_add(1 << 20);
        inserted += v3::publish(storage, family, request, None, |publication| {
            let mut inserted = 0;
            for (key, payload) in &rows[start..end] {
                if publication.put_or_touch(key, payload, schema)? {
                    inserted += 1;
                }
            }
            Ok(inserted)
        })?;
        start = end;
    }
    Ok(inserted)
}

/// Restamps existing rows with the current sweep epoch, so the plane-row
/// sweep keeps them another [`SYMBOL_RETENTION_SWEEPS`] epochs. Returns the
/// keys that have no row.
pub fn touch(
    storage: &Path,
    family: &str,
    keys: &[FamilyKey],
) -> Result<Vec<FamilyKey>, v3::Error> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let request = (keys.len() as u64).saturating_mul(64 * 1024).max(1 << 20);
    v3::publish(storage, family, request, None, |publication| {
        publication.touch(keys)
    })
}

/// Resident lists are keyed by the plane store they were read from as well
/// as the key, so a lookup in one storage root or family never answers from
/// rows another one stored.
type ResidentKey = (PathBuf, [u8; 32]);

struct Residency {
    entries: HashMap<ResidentKey, (Arc<Vec<Symbol>>, u64)>,
    order: VecDeque<ResidentKey>,
    bytes: u64,
}

fn residency() -> &'static Mutex<Residency> {
    static RESIDENT: OnceLock<Mutex<Residency>> = OnceLock::new();
    RESIDENT.get_or_init(|| {
        Mutex::new(Residency {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        })
    })
}

fn resident(key: &ResidentKey) -> Option<Arc<Vec<Symbol>>> {
    residency()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .get(key)
        .map(|(symbols, _)| Arc::clone(symbols))
}

fn make_resident(key: ResidentKey, symbols: Arc<Vec<Symbol>>, bytes: u64) {
    let mut cache = residency()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.entries.contains_key(&key) || bytes > RESIDENT_BYTES {
        return;
    }
    while cache.bytes + bytes > RESIDENT_BYTES {
        let Some(oldest) = cache.order.pop_front() else {
            break;
        };
        if let Some((_, size)) = cache.entries.remove(&oldest) {
            cache.bytes -= size;
        }
    }
    cache.entries.insert(key.clone(), (symbols, bytes));
    cache.order.push_back(key);
    cache.bytes += bytes;
}

/// The stored symbols of the file at `path` whose bytes hash to `content`,
/// under this build's producer, or `None` when the plane has no such row.
/// Never parses: a miss is the caller's to fill.
pub fn lookup(
    storage: &Path,
    family: &str,
    content: ContentHash,
    path: &Path,
) -> Result<Option<Arc<Vec<Symbol>>>, String> {
    let Some(language) = language_of(path) else {
        return Ok(None);
    };
    let producer = SymbolProducer::current();
    let key = SymbolKey {
        content,
        language: language.clone(),
        producer,
    }
    .family_key();
    let plane = v3::plane_path(storage, family, FamilyPlane::Symbol).map_err(|e| e.to_string())?;
    let resident_key = (plane.clone(), *key.as_bytes());
    if let Some(symbols) = resident(&resident_key) {
        return Ok(Some(symbols));
    }
    if !plane.exists() {
        return Ok(None);
    }
    let reader = v3::PlaneReader::open_existing(storage, family, FamilyPlane::Symbol)
        .map_err(|error| error.to_string())?;
    let Some(payload) = reader.get(&key).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let symbols = Arc::new(decode_payload(&payload, &producer, &language)?);
    make_resident(resident_key, Arc::clone(&symbols), payload.len() as u64);
    Ok(Some(symbols))
}

#[cfg(test)]
#[path = "symbol_store_tests.rs"]
mod tests;
