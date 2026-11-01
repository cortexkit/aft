//! Loss-tolerant, bounded delivery of already-sanitized display snapshots.
//! A retry owns its first wire bytes; execution is never repeated for display.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use cortexkit_role_tool_display::{canonical_body, validate_write, DisplayKind, DisplayLimits, DisplayWrite};
use serde_json::{json, Value};
use subc_protocol::Principal;

const MAX_CALLS: usize = 64;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub runner: Principal,
    pub call_key: String,
    pub schema_pin: String,
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub identity: Identity,
    pub kind: DisplayKind,
    pub body: Vec<u8>,
    pub frame: Vec<u8>,
    admitted: Instant,
    pub attempts: u8,
    pub next: Instant,
}

pub(crate) struct Outbox {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl Outbox {
    pub(crate) fn new() -> Self { Self { entries: VecDeque::new(), bytes: 0 } }

    pub(crate) fn admit(&mut self, identity: Identity, kind: DisplayKind, body: Value, limits: &DisplayLimits, now: Instant) -> bool {
        let Some(map) = body.as_object() else { return false; };
        let write = DisplayWrite::new(identity.runner.clone(), &identity.call_key, kind.clone(), &identity.schema_pin, map.clone());
        let Ok(bytes) = validate_write(&write, limits) else { return false; };
        if self.entries.iter().any(|e| e.identity.runner == identity.runner && e.identity.call_key == identity.call_key && e.kind == kind) {
            return false;
        }
        let Ok(frame) = canonical_body(&super::records::object(json!({"op":"display.write","params": write}))) else { return false; };
        if frame.len() >= 4 * 1024 * 1024 { return false; }
        let call_count = self.entries.iter().filter(|e| e.kind == DisplayKind::Record).count();
        if kind == DisplayKind::Record && call_count >= MAX_CALLS { return false; }
        // Details are optional: shed them before rejecting a compact record.
        if kind != DisplayKind::Detail {
            while self.bytes.saturating_add(frame.len()) > MAX_BYTES {
                let Some(index) = self.entries.iter().position(|e| e.kind == DisplayKind::Detail) else { break; };
                if let Some(entry) = self.entries.remove(index) { self.bytes -= entry.frame.len(); }
            }
        }
        if self.entries.len() >= MAX_CALLS * 3 || self.bytes.saturating_add(frame.len()) > MAX_BYTES { return false; }
        self.bytes += frame.len();
        self.entries.push_back(Entry { identity, kind, body: bytes, frame, admitted: now, attempts: 0, next: now });
        true
    }

    pub(crate) fn pop_due(&mut self, now: Instant) -> Option<Entry> {
        self.entries.retain(|entry| {
            let keep = now.duration_since(entry.admitted) < EXPIRY && entry.attempts < 8;
            if !keep { self.bytes -= entry.frame.len(); }
            keep
        });
        let index = self.entries.iter().position(|e| e.next <= now)?;
        let entry = self.entries.remove(index)?;
        self.bytes -= entry.frame.len();
        Some(entry)
    }

    pub(crate) fn retry(&mut self, mut entry: Entry, now: Instant) {
        entry.attempts += 1;
        if entry.attempts >= 8 || now.duration_since(entry.admitted) >= EXPIRY { return; }
        let backoff = (250u64 << entry.attempts.saturating_sub(1)).min(30_000);
        // Stable bounded jitter avoids synchronization without changing payloads.
        let jitter = entry.body.len() as u64 % (backoff / 5 + 1);
        entry.next = now + Duration::from_millis(backoff + jitter);
        if self.bytes + entry.frame.len() <= MAX_BYTES && self.entries.len() < MAX_CALLS * 3 {
            self.bytes += entry.frame.len();
            self.entries.push_back(entry);
        }
    }
}

pub(crate) fn ceilings() -> DisplayLimits { DisplayLimits::new(16_384, 3_145_728, 16_384, 0) }
