//! Content lookup shared by cold builds and call-graph refreshes.

use super::{known_sections, Checkout, DataError, Extracted};
use aft_store::content::{ContentRecord, ContentSegment, EncodedRecord, RecordKey};
use aft_store::family::Snapshot;
use aft_store::segment::Segment;
use std::collections::HashMap;
use std::sync::Arc;

/// A key names the bytes, language, and extractor version, so the stored
/// record is what extraction would produce.
#[derive(Default)]
pub(super) struct StoredRecords {
    /// Keeps the segments' files on disk while readers use them.
    _snapshot: Option<Snapshot>,
    /// Newest last.
    segments: Vec<Arc<Segment>>,
    /// A cold scan also reuses parses from pending changes that an update
    /// extracted but has not published.
    pub(super) pending: HashMap<String, Arc<EncodedRecord>>,
}

impl StoredRecords {
    /// Finds the record in the newest segment holding the key. Each lookup
    /// reads only the index blocks and matching record. Unreadable records
    /// count as misses, so extraction can still use the source bytes.
    pub(super) fn record(&self, key: RecordKey) -> Option<ContentRecord> {
        if !self.pending.is_empty() {
            if let Some(record) = self
                .pending
                .get(&key.digest().to_string())
                .and_then(|encoded| encoded.decode().ok())
            {
                return Some(record);
            }
        }
        self.segments.iter().rev().find_map(|segment| {
            let content = ContentSegment::open(segment).ok().flatten()?;
            let ordinal = content.find(key).ok().flatten()?;
            content.record(ordinal).ok()
        })
    }
}

impl Checkout {
    pub(super) fn stored_records(&self) -> Result<StoredRecords, DataError> {
        let mut stored = self.family_records()?;
        stored.pending = self.ready_change_records();
        Ok(stored)
    }

    /// A cold scan can reuse content already extracted by an aborted update.
    /// Reading this cache must not start parsing: a batch rejected before
    /// extraction should reach the cold scan unparsed. The scan looks up each
    /// record by its on-disk content and extractor key.
    fn ready_change_records(&self) -> HashMap<String, Arc<EncodedRecord>> {
        #[cfg(feature = "test-hooks")]
        if std::env::var_os("PROBE_SKIP_HANDOFF").is_some() {
            return HashMap::new();
        }
        self.live
            .read()
            .expect("no panics while holding it")
            .changes
            .values()
            .filter_map(|change| match change.content.peek() {
                Some(Extracted::Parse(key, _, record)) => Some((key.clone(), Arc::clone(record))),
                _ => None,
            })
            .collect()
    }

    /// Opens the newest generation before extracting any files. Unusable
    /// segments are left out, so their files are parsed instead.
    pub(super) fn family_records(&self) -> Result<StoredRecords, DataError> {
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let known = known_sections();
        let mut segments = Vec::new();
        for entry in snapshot.manifest().segments() {
            match snapshot.open_shared_segment(entry.id, &known, &self.layer.cache) {
                Ok(segment) => segments.push(segment),
                Err(error) if error.is_unusable_file() => {}
                Err(error) => return Err(DataError::store(error)),
            }
        }
        Ok(StoredRecords {
            _snapshot: Some(snapshot),
            segments,
            pending: HashMap::new(),
        })
    }
}
