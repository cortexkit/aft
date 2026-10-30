//! Import of a legacy symbol cache (`symbols/<key>/symbols.bin`) into the v3
//! symbol plane, without parsing.
//!
//! The legacy cache stores, per checkout-relative path, the content hash and
//! size of the bytes it parsed and their symbols. This importer adapts the
//! entries the disk cache reader validates (`DecodedSymbolCache`): the path
//! must stay under the current root and the file there must still be the
//! recorded bytes (same size, same hash). Each such entry becomes a row keyed
//! by content, language and the extraction schema (`views::symbol_store`),
//! so any checkout of the family with the same bytes reuses it. A cache of
//! another format or extraction schema yields nothing: a parser or schema
//! change misses rather than reusing stale symbols.
//!
//! The import reads the legacy cache once for its hash and once to convert
//! it, through bounded reads, and reads each listed source file to verify its
//! hash. It never calls the parser.

use std::fs;
use std::io::BufReader;
use std::path::Path;

use super::per_checkout::BundleEntry;
use super::v3_import::Source;
use crate::blob_store::v2::ContentHash;
use crate::symbols::Symbol;
use crate::views::symbol_store::{self, SymbolKey, SymbolProducer};
use crate::views::RelPath;

/// Source files larger than this are not read to verify a cached entry; the
/// symbol plane fills them on demand instead.
const MAX_SYMBOL_SOURCE_BYTES: u64 = 16 << 20;
const READ_CHUNK: usize = 64 * 1024;

/// Converts a legacy `symbols.bin` into symbol-plane entries, handing each
/// to `emit`. Returns the file's BLAKE3 and the number of entries left for
/// the plane to fill (path outside the root, unknown language, source
/// changed or unreadable, undecodable symbols).
pub(super) fn convert_symbols_streaming(
    source: &mut Source,
    root: &Path,
    emit: &mut dyn FnMut(BundleEntry) -> Result<(), String>,
) -> Result<([u8; 32], u64), String> {
    let (source_hash, _) = source.checksum_pass(&[])?;
    let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let producer = SymbolProducer::current();
    let mut canonical_parents = crate::search_index::ParentCanonicalizationMemo::default();
    let mut skipped = 0_u64;
    let reader = BufReader::with_capacity(READ_CHUNK, source.sequential());
    crate::symbol_cache_disk::for_each_raw_entry(reader, &mut |raw| {
        let Some(language) = symbol_store::language_of(&raw.relative_path) else {
            skipped += 1;
            return Ok(());
        };
        // The validated relative entry: it maps under the current root, and
        // the file there still holds the bytes the symbols were parsed from.
        let Some(path) = crate::search_index::cached_path_under_root_with_memo(
            &root,
            &raw.relative_path,
            &mut canonical_parents,
        ) else {
            skipped += 1;
            return Ok(());
        };
        let bytes = fs::symlink_metadata(&path)
            .ok()
            .filter(|meta| {
                meta.is_file() && meta.len() == raw.size && meta.len() <= MAX_SYMBOL_SOURCE_BYTES
            })
            .and_then(|_| fs::read(&path).ok());
        let Some(bytes) = bytes.filter(|bytes| blake3::hash(bytes) == raw.content_hash) else {
            skipped += 1;
            return Ok(());
        };
        let Ok(symbols) = serde_json::from_slice::<Vec<Symbol>>(&raw.symbols_json) else {
            skipped += 1;
            return Ok(());
        };
        let Ok(rel_path) = RelPath::from_os_path(&raw.relative_path) else {
            skipped += 1;
            return Ok(());
        };
        let content = ContentHash::of(&bytes);
        let key = SymbolKey {
            content,
            language: language.clone(),
            producer,
        }
        .family_key();
        emit(BundleEntry {
            rel_path,
            content,
            size: raw.size,
            key: *key.as_bytes(),
            payload: symbol_store::encode_payload(&producer, &language, &symbols),
        })
    })?;
    Ok((source_hash, skipped))
}

/// Re-derives a staged symbol entry's key from its content and path and
/// checks that its payload decodes for that language and this producer.
pub(super) fn check_staged_entry(entry: &BundleEntry) -> Result<(), String> {
    let relative = crate::views::segment_store::rel_path_to_os(&entry.rel_path)
        .map_err(|error| error.to_string())?;
    let language =
        symbol_store::language_of(&relative).ok_or("staged symbol entry has no language")?;
    let producer = SymbolProducer::current();
    symbol_store::decode_payload(&entry.payload, &producer, &language)?;
    let key = SymbolKey {
        content: entry.content,
        language,
        producer,
    }
    .family_key();
    (key.as_bytes() == &entry.key)
        .then_some(())
        .ok_or_else(|| "staged symbol key differs from its content".to_owned())
}

#[cfg(test)]
#[path = "v3_symbols_tests.rs"]
mod tests;
