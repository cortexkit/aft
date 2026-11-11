//! Stateless text primitives shared by shell persistence and reply renderers.

/// Port of `packages/aft-bridge/src/bash-format.ts` `formatSeconds` (lines 3-6).
pub fn format_seconds(ms: u64) -> String {
    let mut seconds = format!("{:.1}", ms as f64 / 1000.0);
    if seconds.ends_with(".0") {
        seconds.truncate(seconds.len() - 2);
    }
    format!("{seconds}s")
}

/// A wait limit in words: whole minutes as minutes, anything else in seconds.
pub(crate) fn format_wait_limit(ms: u64) -> String {
    match ms {
        60_000 => "1 minute".to_string(),
        ms if ms > 0 && ms % 60_000 == 0 => format!("{} minutes", ms / 60_000),
        ms => format_seconds(ms),
    }
}

pub(crate) fn append_trailing_line(text: &mut String, line: &str) {
    if !text.is_empty() {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(line);
}

/// Schema lookups are AFT-added trailers, not command output. Both terminal
/// snapshots and completion previews call this after their output size caps.
#[cfg(unix)]
pub(crate) fn append_db_schema_hint(text: &mut String, hint: &str) {
    append_trailing_line(text, hint);
}
