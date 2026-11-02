use std::path::Path;

use serde::Serialize;

pub(crate) const CODE: &str = "device_free_space_floor";

/// Approved by Ufuk: agent-facing wording for a measured disk-reserve refusal.
pub(crate) const APPROVED_MESSAGE: &str = "View storage is paused: {free} is free on this disk and AFT keeps {floor} free. Unused view data is being cleaned up; retry later.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskReserveCondition {
    pub code: &'static str,
    pub free_bytes: u64,
    pub floor_bytes: u64,
    pub message: String,
}

impl DiskReserveCondition {
    pub(crate) fn observed(storage: &Path, error: &str) -> Option<Self> {
        if !error.contains(CODE) && !is_message(error) {
            return None;
        }
        // A failed probe has no sizes to disclose. Keep its unexpected-error
        // treatment rather than inventing a free-space measurement.
        let (free_bytes, capacity) = crate::blob_store::v3::free_space(storage)?;
        let floor_bytes = crate::blob_store::v3::free_disk_floor(capacity);
        let human_bytes = crate::migration::prune_legacy::human_bytes;
        let message = APPROVED_MESSAGE
            .replace("{free}", &human_bytes(free_bytes))
            .replace("{floor}", &human_bytes(floor_bytes));
        Some(Self {
            code: CODE,
            free_bytes,
            floor_bytes,
            message,
        })
    }
}

pub(crate) fn is_message(message: &str) -> bool {
    message.starts_with("View storage is paused: ")
        && message.ends_with("Unused view data is being cleaned up; retry later.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_reserve_approved_sentence_is_pinned_byte_for_byte() {
        assert_eq!(APPROVED_MESSAGE, "View storage is paused: {free} is free on this disk and AFT keeps {floor} free. Unused view data is being cleaned up; retry later.");
        let storage = tempfile::tempdir().unwrap();
        let _space = crate::blob_store::v3::set_free_space_for_test(
            storage.path(),
            21 * 1024 * 1024 * 1024,
            3_900_000_000_000,
        );
        let condition = DiskReserveCondition::observed(storage.path(), CODE).unwrap();
        assert_eq!(condition.message, "View storage is paused: 21.0 GiB (22548578304 bytes) is free on this disk and AFT keeps 32.0 GiB (34359738368 bytes) free. Unused view data is being cleaned up; retry later.");
    }
}
