//! Process-local pull copies, independent of ownership during a push attempt.
//! Announced records also have a protected database fallback in display::store.
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MIN_READ_WINDOW: Duration = Duration::from_secs(10 * 60);

struct ReadEntry {
    arguments: Vec<u8>,
    weight: usize,
    admitted: Instant,
    push_live: bool,
    announced: Option<Instant>,
    next: Instant,
    delay: Duration,
}

impl ReadEntry {
    fn readable_at(&self, now: Instant) -> bool {
        let before_expiry = now.duration_since(self.admitted) < super::EXPIRY;
        match self.announced {
            Some(at) => (self.push_live && before_expiry) || now <= at + MIN_READ_WINDOW,
            None => before_expiry,
        }
    }
}

#[derive(Default)]
pub(crate) struct ReadCache {
    entries: HashMap<String, ReadEntry>,
    bytes: usize,
}

impl ReadCache {
    pub(in crate::subc::rich_ui) fn insert(
        &mut self,
        id: String,
        arguments: Vec<u8>,
        weight: usize,
        now: Instant,
    ) {
        self.prune(now);
        // Preserve the first bytes and announcement when an identical key is
        // offered again; a push retry must not reset the pull retention window.
        if self.entries.contains_key(&id) {
            return;
        }
        while self.entries.len() >= super::MAX_CALLS * 3
            || self.bytes.saturating_add(weight) > super::MAX_BYTES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.admitted)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            let reason = if self.entries.len() >= super::MAX_CALLS * 3 {
                "display_read_count"
            } else {
                "display_read_bytes"
            };
            self.remove(&oldest, now, Some(reason));
        }
        self.bytes += weight;
        self.entries.insert(
            id,
            ReadEntry {
                arguments,
                weight,
                admitted: now,
                push_live: true,
                announced: None,
                next: now,
                delay: Duration::from_secs(1),
            },
        );
    }

    fn remove(&mut self, id: &str, now: Instant, reason: Option<&'static str>) {
        if let Some(entry) = self.entries.remove(id) {
            self.bytes -= entry.weight;
            if entry.announced.is_some_and(|at| now < at + MIN_READ_WINDOW) {
                if let Some(reason) = reason {
                    crate::late_result_bus::display_read_evicted_early(reason);
                }
            }
        }
    }

    pub(super) fn shed(&mut self, id: &str, now: Instant) {
        self.remove(id, now, Some("display_detail_shed"));
    }

    pub(crate) fn push_finished(&mut self, id: &str, now: Instant) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.push_live = false;
        }
        self.prune(now);
    }

    pub(crate) fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            let keep = entry.readable_at(now);
            if !keep {
                self.bytes -= entry.weight;
            }
            keep
        });
    }

    pub(crate) fn read(&self, id: &str, now: Instant) -> Option<Vec<u8>> {
        let entry = self.entries.get(id)?;
        entry.readable_at(now).then(|| entry.arguments.clone())
    }

    pub(crate) fn due(&mut self, now: Instant) -> Option<String> {
        self.prune(now);
        self.entries
            .iter()
            .filter(|(_, entry)| entry.announced.is_none() && entry.next <= now)
            .min_by_key(|(_, entry)| entry.next)
            .map(|(id, _)| id.clone())
    }

    pub(crate) fn published(&mut self, id: &str, success: bool, now: Instant) {
        if let Some(entry) = self.entries.get_mut(id) {
            if success {
                entry.announced = Some(now);
            } else {
                entry.next = now + entry.delay;
                entry.delay = (entry.delay * 2).min(Duration::from_secs(60));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subc::rich_ui::outbox::{ceilings, Identity, Outbox};
    use cortexkit_role_tool_display::{canonical_body, DisplayKind, DisplayWrite};
    use serde_json::json;
    use subc_protocol::Principal;

    fn admit(outbox: &mut Outbox, key: &str, kind: DisplayKind, now: Instant) -> String {
        assert!(outbox.admit(
            Identity {
                runner: Principal::Reserved {
                    module_id: "runner".into()
                },
                call_key: key.into(),
                schema_pin: "pin:1".into(),
            },
            kind,
            json!({"durationMs":1,"outcome":"success"}),
            &ceilings(),
            now
        ));
        outbox.entries.back().unwrap().record_id.clone()
    }

    #[test]
    fn display_read_has_exact_push_arguments_for_every_kind_and_is_readable_mid_push() {
        let now = Instant::now();
        for kind in [
            DisplayKind::Record,
            DisplayKind::Detail,
            DisplayKind::Completion,
        ] {
            let mut outbox = Outbox::new();
            let id = admit(&mut outbox, "call:1", kind, now);
            assert_eq!(id.len(), 64);
            assert!(id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            let entry = outbox.pop_due(now).unwrap();
            assert!(
                outbox.entries.is_empty(),
                "push owns the entry outside the queue"
            );
            let pushed: serde_json::Value = serde_json::from_slice(&entry.frame).unwrap();
            let expected = canonical_body(pushed["arguments"].as_object().unwrap()).unwrap();
            let pulled = outbox.reads.read(&id, now).unwrap();
            assert_eq!(pulled, expected);
            let write: DisplayWrite = serde_json::from_slice(&pulled).unwrap();
            if write.kind == DisplayKind::Record {
                assert_eq!(
                    id,
                    "e378248bc2b0cea6dba1b30089c21b77e5de09acf83952b4871d8efa8af398da"
                );
            }
            cortexkit_role_tool_display::validate_write(&write, &ceilings()).unwrap();
            assert_eq!(
                cortexkit_role_tool_display::idempotency_key(
                    &Principal::Reserved {
                        module_id: "aft".into()
                    },
                    &write,
                )
                .map(|bytes| {
                    use sha2::Digest;
                    format!("{:x}", sha2::Sha256::digest(bytes))
                })
                .unwrap(),
                id
            );
        }
    }

    #[test]
    fn display_read_survives_success_for_ten_minutes_after_announcement_then_becomes_unknown() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        let id = admit(&mut outbox, "call:1", DisplayKind::Record, now);
        let _in_flight = outbox.pop_due(now).unwrap();
        outbox.reads.push_finished(&id, now);
        // A fast push may finish before the bus has acknowledged its wake.
        let announced = now + Duration::from_secs(30);
        outbox.reads.published(&id, true, announced);
        let boundary = announced + MIN_READ_WINDOW;
        assert!(outbox.reads.read(&id, boundary).is_some());
        assert!(outbox
            .reads
            .read(&id, boundary + Duration::from_nanos(1))
            .is_none());
        outbox.reads.prune(boundary + Duration::from_nanos(1));
        assert_eq!(outbox.reads.bytes, 0);
    }

    #[test]
    fn display_read_outlives_the_minimum_while_push_is_live_but_not_expiry() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        let id = admit(&mut outbox, "long", DisplayKind::Record, now);
        outbox.reads.published(&id, true, now);
        assert!(outbox.reads.read(&id, now + MIN_READ_WINDOW * 2).is_some());
        assert!(outbox.reads.read(&id, now + super::super::EXPIRY).is_none());
    }

    #[test]
    fn display_read_unannounced_entries_expire_even_if_push_is_live() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        let finished = admit(&mut outbox, "finished", DisplayKind::Record, now);
        let live = admit(&mut outbox, "live", DisplayKind::Record, now);
        let announced = admit(&mut outbox, "announced", DisplayKind::Record, now);
        outbox.reads.push_finished(&finished, now);
        let expiry = now + super::super::EXPIRY;
        let before_expiry = expiry - Duration::from_nanos(1);
        outbox.reads.published(&announced, true, before_expiry);
        assert!(outbox.reads.read(&finished, before_expiry).is_some());
        assert!(outbox.reads.read(&live, before_expiry).is_some());
        assert!(outbox.reads.read(&finished, expiry).is_none());
        assert!(outbox.reads.read(&live, expiry).is_none());
        assert!(outbox.reads.due(expiry).is_none());
        assert_eq!(outbox.reads.entries.len(), 1);
        assert!(outbox.reads.read(&announced, expiry).is_some());
    }

    #[test]
    fn display_read_early_eviction_is_counted_for_count_and_byte_pressure() {
        let now = Instant::now();
        let before = crate::late_result_bus::health()["display_read_evicted_early"]
            .as_u64()
            .unwrap();
        for (count, weight) in [
            (super::super::MAX_CALLS * 3, 1),
            (2, super::super::MAX_BYTES / 2),
        ] {
            let mut cache = ReadCache::default();
            for index in 0..count {
                let at = now + Duration::from_nanos(index as u64);
                cache.insert(index.to_string(), b"{}".to_vec(), weight, at);
                cache.published(&index.to_string(), true, at);
            }
            cache.insert(
                "new".into(),
                b"{}".to_vec(),
                weight,
                now + Duration::from_secs(1),
            );
            assert!(cache.read("0", now + Duration::from_secs(1)).is_none());
            assert!(cache.bytes <= super::super::MAX_BYTES);
            assert!(cache.entries.len() <= super::super::MAX_CALLS * 3);
        }
        let after = crate::late_result_bus::health()["display_read_evicted_early"]
            .as_u64()
            .unwrap();
        assert!(after >= before + 2);
    }

    #[test]
    fn display_read_rejected_admission_never_announces_and_missing_bus_never_blocks_push() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        assert!(outbox.reads.due(now).is_none());
        assert!(!outbox.admit(
            Identity {
                runner: Principal::Reserved {
                    module_id: "runner".into()
                },
                call_key: "bad key".into(),
                schema_pin: "pin:1".into()
            },
            DisplayKind::Record,
            json!({}),
            &ceilings(),
            now
        ));
        assert!(outbox.reads.due(now).is_none());
        let id = admit(&mut outbox, "call:1", DisplayKind::Record, now);
        assert_eq!(outbox.reads.due(now), Some(id.clone()));
        outbox.reads.published(&id, false, now);
        assert!(outbox.reads.due(now).is_none());
        assert_eq!(
            outbox.reads.due(now + Duration::from_secs(1)),
            Some(id.clone())
        );
        assert!(
            outbox.pop_due(now).is_some(),
            "bus failure never gates the push"
        );
        assert!(outbox.reads.read(&id, now).is_some());
    }

    #[test]
    fn display_read_restart_has_no_memory_records_or_replay() {
        let now = Instant::now();
        let mut outbox = Outbox::new();
        let id = admit(&mut outbox, "call:1", DisplayKind::Record, now);
        outbox.reads.published(&id, true, now);
        let mut restarted = Outbox::new();
        assert!(restarted.reads.read(&id, now).is_none());
        assert!(restarted.reads.due(now).is_none());
    }
}
