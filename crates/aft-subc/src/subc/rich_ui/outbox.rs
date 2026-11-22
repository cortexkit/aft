//! Loss-tolerant, bounded delivery of already-sanitized display snapshots.
//! A retry owns its first wire bytes; execution is never repeated for display.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use cortexkit_role_tool_display::{
    canonical_body, validate_write, DisplayKind, DisplayLimits, DisplayWrite,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_protocol::Principal;

use super::display::counters::{count, Counter::*};

mod reads;
pub(crate) use reads::ReadCache;

const MAX_CALLS: usize = 64;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) use crate::bash_background::frames::Identity;

#[derive(Clone)]
pub(crate) struct Entry {
    pub record_id: String,
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
    pub reads: ReadCache,
}

impl Outbox {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            reads: ReadCache::default(),
        }
    }

    pub(crate) fn admit(
        &mut self,
        identity: Identity,
        kind: DisplayKind,
        body: Value,
        limits: &DisplayLimits,
        now: Instant,
    ) -> bool {
        let Some(map) = body.as_object() else {
            count(AdmitNotObject);
            return false;
        };
        let write = DisplayWrite::new(
            identity.runner.clone(),
            &identity.call_key,
            kind.clone(),
            &identity.schema_pin,
            map.clone(),
        );
        let Ok(bytes) = validate_write(&write, limits) else {
            count(AdmitInvalidWrite);
            return false;
        };
        let Ok(key) = cortexkit_role_tool_display::idempotency_key(
            &Principal::Reserved {
                module_id: "aft".into(),
            },
            &write,
        ) else {
            count(AdmitInvalidKey);
            return false;
        };
        let record_id = format!("{:x}", Sha256::digest(key));
        let Ok(arguments) = canonical_body(&super::records::object(json!(write))) else {
            count(AdmitArgumentsEncoding);
            return false;
        };
        if self.entries.iter().any(|e| {
            e.identity.runner == identity.runner
                && e.identity.call_key == identity.call_key
                && e.kind == kind
        }) {
            count(AdmitDuplicate);
            return false;
        }
        let Ok(frame) = canonical_body(&super::records::object(
            json!({"name":"display.write","arguments": write}),
        )) else {
            count(AdmitFrameEncoding);
            return false;
        };
        if frame.len() >= 4 * 1024 * 1024 {
            count(AdmitFrameTooLarge);
            return false;
        }
        let calls: std::collections::HashSet<_> = self
            .entries
            .iter()
            .map(|entry| {
                (
                    serde_json::to_string(&entry.identity.runner).expect("principal"),
                    entry.identity.call_key.clone(),
                )
            })
            .collect();
        let this_call = (
            serde_json::to_string(&identity.runner).expect("principal"),
            identity.call_key.clone(),
        );
        if !calls.contains(&this_call) && calls.len() >= MAX_CALLS {
            count(AdmitCallCap);
            return false;
        }
        let weight = frame.len().saturating_add(bytes.len());
        // Details are optional: shed them before rejecting a compact record.
        if kind != DisplayKind::Detail {
            while self.bytes.saturating_add(weight) > MAX_BYTES {
                let Some(index) = self
                    .entries
                    .iter()
                    .position(|e| e.kind == DisplayKind::Detail)
                else {
                    break;
                };
                if let Some(entry) = self.entries.remove(index) {
                    count(AdmitShedDetail);
                    self.bytes -= entry.frame.len() + entry.body.len();
                    self.reads.shed(&entry.record_id, now);
                }
            }
        }
        if self.entries.len() >= MAX_CALLS * 3 {
            count(AdmitEntryCap);
            return false;
        }
        if self.bytes.saturating_add(weight) > MAX_BYTES {
            count(AdmitByteCap);
            return false;
        }
        self.bytes += weight;
        self.entries.push_back(Entry {
            record_id: record_id.clone(),
            identity,
            kind,
            body: bytes,
            frame,
            admitted: now,
            attempts: 0,
            next: now,
        });
        // Queue admission is the process-local commit point. Only committed
        // snapshots become readable or eligible for a bus wake.
        self.reads.insert(record_id, arguments, weight, now);
        count(Admitted);
        true
    }

    pub(crate) fn pop_due(&mut self, now: Instant) -> Option<Entry> {
        let reads = &mut self.reads;
        self.entries.retain(|entry| {
            let keep = now.duration_since(entry.admitted) < EXPIRY && entry.attempts < 8;
            if !keep {
                count(if now.duration_since(entry.admitted) >= EXPIRY {
                    Expired
                } else {
                    AttemptsExhausted
                });
                self.bytes -= entry.frame.len() + entry.body.len();
                reads.push_finished(&entry.record_id, now);
            }
            keep
        });
        let index = self.entries.iter().position(|e| e.next <= now)?;
        let entry = self.entries.remove(index)?;
        self.bytes -= entry.frame.len() + entry.body.len();
        Some(entry)
    }

    pub(crate) fn retry(&mut self, mut entry: Entry, now: Instant) {
        entry.attempts += 1;
        if entry.attempts >= 8 || now.duration_since(entry.admitted) >= EXPIRY {
            count(if entry.attempts >= 8 {
                AttemptsExhausted
            } else {
                Expired
            });
            self.reads.push_finished(&entry.record_id, now);
            return;
        }
        let backoff = (250u64 << entry.attempts.saturating_sub(1)).min(30_000);
        // Stable bounded jitter avoids synchronization without changing payloads.
        let jitter = entry.body.len() as u64 % (backoff / 5 + 1);
        entry.next = now + Duration::from_millis(backoff + jitter);
        if self.bytes + entry.frame.len() + entry.body.len() <= MAX_BYTES
            && self.entries.len() < MAX_CALLS * 3
        {
            self.bytes += entry.frame.len() + entry.body.len();
            self.entries.push_back(entry);
            count(Retried);
        } else {
            count(RetryCapacity);
            self.reads.push_finished(&entry.record_id, now);
        }
    }
}

pub(crate) fn ceilings() -> DisplayLimits {
    DisplayLimits::new(
        16_384,
        crate::bash_background::frames::MAX_BASH_DETAIL_BYTES,
        16_384,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(key: &str) -> Identity {
        Identity {
            runner: Principal::Reserved {
                module_id: "broca".into(),
            },
            call_key: key.into(),
            schema_pin: crate::subc::tool_views::SHIPPED
                .keys()
                .next()
                .unwrap()
                .clone(),
        }
    }
    fn record() -> Value {
        json!({"durationMs":8,"outcome":"success"})
    }

    #[test]
    fn ack_loss_retries_the_first_canonical_bytes_not_a_rebuilt_snapshot() {
        let mut outbox = Outbox::new();
        let now = Instant::now();
        assert!(outbox.admit(
            identity("original"),
            DisplayKind::Record,
            record(),
            &ceilings(),
            now
        ));
        let first = outbox.pop_due(now).unwrap();
        let stored = first.body.clone();
        let frame = first.frame.clone();
        // The fake store commits, but the acknowledgement is lost.
        assert!(matches!(
            cortexkit_role_tool_display::write_outcome(None, &stored),
            Ok(cortexkit_role_tool_display::WriteReply::Stored)
        ));
        outbox.retry(first, now);
        let retry = outbox.pop_due(now + Duration::from_secs(1)).unwrap();
        assert_eq!(retry.body, stored);
        assert_eq!(retry.frame, frame);
        assert!(matches!(
            cortexkit_role_tool_display::write_outcome(Some(&stored), &retry.body),
            Ok(cortexkit_role_tool_display::WriteReply::AlreadyStored)
        ));
        let envelope: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(envelope["name"], "display.write");
        assert_eq!(
            envelope["arguments"]["runner"],
            json!({"kind":"reserved","module_id":"broca"})
        );
    }

    #[test]
    fn outbox_bounds_calls_bytes_retries_and_expiry_without_reexecuting() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        for index in 0..64 {
            assert!(outbox.admit(
                identity(&format!("k{index}")),
                DisplayKind::Record,
                record(),
                &ceilings(),
                now
            ));
        }
        assert!(!outbox.admit(
            identity("overflow"),
            DisplayKind::Record,
            record(),
            &ceilings(),
            now
        ));
        assert!(outbox.pop_due(now + EXPIRY).is_none());
        let mut outbox = Outbox::new();
        assert!(outbox.admit(
            identity("retry"),
            DisplayKind::Record,
            record(),
            &ceilings(),
            now
        ));
        for attempt in 0..8 {
            let at = now + Duration::from_secs(attempt * 40);
            let entry = outbox.pop_due(at).unwrap();
            assert_eq!(entry.attempts as u64, attempt);
            outbox.retry(entry, at);
        }
        assert!(outbox.pop_due(now + Duration::from_secs(1000)).is_none());
        assert_eq!(outbox.bytes, 0);
    }

    #[test]
    fn pressure_sheds_optional_details_first() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        for index in 0..2 {
            assert!(outbox.admit(
                identity(&format!("k{index}")),
                DisplayKind::Detail,
                json!({"text":"x".repeat(3_000_000)}),
                &ceilings(),
                now
            ));
        }
        let remaining = MAX_BYTES - outbox.bytes;
        assert!(outbox.admit(
            identity("filler"),
            DisplayKind::Detail,
            json!({"text":"x".repeat(remaining / 2 - 1000)}),
            &ceilings(),
            now
        ));
        let before = outbox
            .entries
            .iter()
            .filter(|entry| entry.kind == DisplayKind::Detail)
            .count();
        let shed_id = outbox.entries.front().unwrap().record_id.clone();
        outbox.reads.published(&shed_id, true, now);
        assert!(outbox.reads.read(&shed_id, now).is_some());
        assert!(outbox.admit(
            identity("record"),
            DisplayKind::Record,
            json!({"durationMs":8,"outcome":"success","path":vec!["x".repeat(1000);14]}),
            &ceilings(),
            now
        ));
        assert_eq!(
            outbox
                .entries
                .iter()
                .filter(|entry| entry.kind == DisplayKind::Detail)
                .count(),
            before - 1
        );
        assert!(outbox.bytes <= MAX_BYTES);
        assert!(
            outbox.reads.read(&shed_id, now).is_none(),
            "shed details are immediately unknown"
        );
    }
}
