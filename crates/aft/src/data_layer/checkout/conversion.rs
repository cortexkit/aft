//! Conversion of extracted content records to call-graph inputs.

use super::super::{content, grammar};
use super::{staged_records, Extracted, RecordedSource, StoredRecords};
use crate::callgraph_store::join::{CallgraphBlob, ParseBlob};
use aft_store::content::{ContentRecord, EncodedRecord, RecordBody, RecordKey};
use std::sync::Arc;

pub(super) fn parse_record(record: &ContentRecord) -> Result<ParseBlob, String> {
    match content::blob_from_record(record).map_err(|error| error.to_string())? {
        CallgraphBlob::Parse(parse) => Ok(parse),
        _ => Err("a parsed content record did not produce a parse".into()),
    }
}

pub(super) fn warn_conversion_failure(path: &str, reason: &str) {
    crate::slog_warn!("call-graph extraction failed for {path}: {reason}");
}

/// The staging table saves only whether conversion worked. Read back the
/// reason for a failed record, including when resuming an interrupted build.
pub(super) fn staged_failure(
    records: &staged_records::StagedRecords,
    at: staged_records::StagedAt,
    path: &str,
) -> String {
    let Some(record) = records.encoded(at, path) else {
        return "could not read the failed staged record".into();
    };
    record
        .decode()
        .map_err(|error| error.to_string())
        .and_then(|record| parse_record(&record))
        .err()
        .unwrap_or_else(|| "the staging table recorded a failed conversion".into())
}

/// What the call graph reads from a changed file. As in a cold build, a
/// file whose record cannot be converted has no parse.
pub(super) fn extract_recorded(source: &RecordedSource, stored: &StoredRecords) -> Extracted {
    let key = content::record_key(source.content, &source.language);
    if let Some(record) = stored.record(key) {
        // The stored record already includes config TODOs. The key's
        // language includes the grammar that read them.
        if let Ok(extracted) = try_convert_recorded(key, &record) {
            return extracted;
        }
    }
    let mut extracted = match grammar::extract(&source.bytes, &source.language) {
        Ok(record) => record,
        Err(error) => {
            return grammar::reason(&error).map_or(Extracted::Nothing, Extracted::GrammarGap)
        }
    };
    if let Ok(path) = std::str::from_utf8(&source.path) {
        // Without them a configuration file's TODOs are reported unread.
        let _ = content::add_config_markers(&mut extracted, path, &source.bytes);
    }
    convert_recorded(&source.path, key, &extracted)
}

fn convert_recorded(path: &[u8], key: RecordKey, record: &ContentRecord) -> Extracted {
    try_convert_recorded(key, record).unwrap_or_else(|reason| {
        warn_conversion_failure(&String::from_utf8_lossy(path), &reason);
        Extracted::ConversionFailed
    })
}

fn try_convert_recorded(key: RecordKey, record: &ContentRecord) -> Result<Extracted, String> {
    Ok(match &record.data {
        RecordBody::Config { source } => {
            let source = Arc::from(source.as_slice());
            Extracted::Config(source, Arc::new(EncodedRecord::of(record)))
        }
        RecordBody::Markers => Extracted::Markers(Arc::new(EncodedRecord::of(record))),
        RecordBody::Parsed { .. } => Extracted::Parse(
            key.digest().to_string(),
            Arc::new(parse_record(record)?),
            Arc::new(EncodedRecord::of(record)),
        ),
    })
}

#[cfg(test)]
mod tests;
