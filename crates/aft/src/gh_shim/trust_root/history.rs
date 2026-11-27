use super::Pin;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io;

const PAST_KEYS: usize = 16;

fn valid_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct History {
    pub(super) generation: u64,
    current_key_id: Option<String>,
    previous_key_ids: VecDeque<String>,
}

impl History {
    fn remember(&mut self, id: String) {
        if let Some(index) = self.previous_key_ids.iter().position(|seen| seen == &id) {
            self.previous_key_ids.remove(index);
        }
        // Keep the most recent superseded or retired keys. Once a key falls
        // out of this history it counts as never seen for rollback recovery.
        if self.previous_key_ids.len() == PAST_KEYS {
            self.previous_key_ids.pop_front();
        }
        self.previous_key_ids.push_back(id);
    }

    pub(super) fn valid(&self) -> bool {
        self.previous_key_ids.len() <= PAST_KEYS
            && self
                .current_key_id
                .iter()
                .chain(&self.previous_key_ids)
                .all(|id| valid_id(id))
    }

    pub(super) fn had_pin(&self) -> bool {
        self.current_key_id.is_some() || !self.previous_key_ids.is_empty()
    }

    pub(super) fn merge(&mut self, disk: &Self) {
        if self == disk {
            return;
        }
        // Preserve the higher floor and the newest known active key when a
        // second publisher has written a newer marker under the same lock.
        if disk.generation > self.generation {
            let memory = std::mem::replace(self, disk.clone());
            for id in memory.previous_key_ids.into_iter().rev() {
                if !self.previous_key_ids.contains(&id) && self.previous_key_ids.len() < PAST_KEYS {
                    self.previous_key_ids.push_front(id);
                }
            }
        } else {
            for id in disk.previous_key_ids.iter().rev() {
                if !self.previous_key_ids.contains(id) && self.previous_key_ids.len() < PAST_KEYS {
                    self.previous_key_ids.push_front(id.clone());
                }
            }
        }
    }

    /// Advance the rollback floor, returning whether a fresh identity reset it.
    pub(super) fn advance(&mut self, pin: Option<&Pin>, generation: u64) -> io::Result<bool> {
        let key = pin.map(|pin| pin.key_id.as_str());
        let lower = generation < self.generation;
        let previously_retired = key.is_some_and(|key| {
            self.previous_key_ids.iter().any(|id| id == key)
                || pin.is_some_and(|pin| pin.retired_key_ids.iter().any(|id| id == key))
        });
        if lower && previously_retired {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "policy root generation regressed to a superseded or retired key",
            ));
        }
        let reset = lower && key.is_some() && key != self.current_key_id.as_deref();
        if !lower || reset {
            self.generation = generation;
        }
        if key != self.current_key_id.as_deref() {
            if let Some(previous) = self.current_key_id.take() {
                self.remember(previous);
            }
            self.current_key_id = key.map(str::to_owned);
        }
        if let Some(pin) = pin {
            for id in &pin.retired_key_ids {
                if valid_id(id) {
                    self.remember(id.clone());
                }
            }
        }
        Ok(reset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_shim_keys_evicted_from_bounded_history_count_as_never_seen() {
        let mut history = History::default();
        let pins: Vec<_> = (0..=PAST_KEYS + 1)
            .map(|seed| crate::gh_shim::tests::pin_for(&[seed as u8; 32]))
            .collect();
        for (version, pin) in pins.iter().enumerate() {
            history.advance(Some(pin), version as u64).unwrap();
        }
        assert_eq!(history.previous_key_ids.len(), PAST_KEYS);
        assert!(history.advance(Some(&pins[1]), 0).is_err());
        assert!(history.advance(Some(&pins[0]), 0).unwrap());
        assert_eq!(history.generation, 0);
    }
}
