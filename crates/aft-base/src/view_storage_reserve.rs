use std::path::Path;

use serde::Serialize;

pub(crate) const CODE: &str = "device_free_space_floor";

/// Approved by Ufuk: agent-facing wording for a measured disk-reserve refusal.
pub(crate) const APPROVED_MESSAGE: &str = "Index storage is paused: {free} is free on this disk and AFT keeps {floor} free. Unused index data is being cleaned up; retry later.";

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
        let (free_bytes, capacity) = crate::disk_floor::floor_refusal_measurement(storage)?;
        let floor_bytes = crate::disk_floor::free_disk_floor(capacity);
        Some(Self {
            code: CODE,
            free_bytes,
            floor_bytes,
            message: message(free_bytes, floor_bytes),
        })
    }
}

/// The approved pause sentence for `free_bytes` free against a floor of
/// `floor_bytes`.
pub(crate) fn message(free_bytes: u64, floor_bytes: u64) -> String {
    APPROVED_MESSAGE
        .replace("{free}", &size(free_bytes))
        .replace("{floor}", &size(floor_bytes))
}

/// A size for the agent-facing sentence, such as `21.0 GiB`. The exact byte
/// counts are in `free_bytes` and `floor_bytes` for anything that needs them.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn is_message(message: &str) -> bool {
    message.starts_with("Index storage is paused: ")
        && message.ends_with("Unused index data is being cleaned up; retry later.")
}

#[cfg(test)]
mod tests {}
