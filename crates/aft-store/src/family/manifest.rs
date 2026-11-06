//! The manifest: the record that publishes one generation of a family.
//!
//! A manifest is the family's commit record. It names every segment of its
//! generation and, for every checkout, the one state file current at that
//! generation. A reader takes all of a snapshot from one manifest, so it never
//! combines a segment set with a checkout state from another generation.

use super::record::{Payload, RecordError};
use crate::ids::{CheckoutId, Digest128, Generation, SegmentId};

/// One segment a manifest names, with the length it was published at. A
/// segment file of any other length is damaged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentEntry {
    pub id: SegmentId,
    pub len: u64,
}

/// One checkout's current state file, as a manifest names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateRef {
    pub checkout: CheckoutId,
    /// The number in the state's file name, `STATE-<number>`.
    pub number: u64,
    /// The length and digest of the state's payload.
    pub len: u64,
    pub digest: Digest128,
}

/// One published generation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Manifest {
    generation: Generation,
    created_unix_ms: u64,
    next_segment: SegmentId,
    segments: Vec<SegmentEntry>,
    checkouts: Vec<StateRef>,
}

impl Manifest {
    /// The generation of a family that has never published.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            generation: Generation::EMPTY,
            created_unix_ms: 0,
            next_segment: SegmentId::new(1),
            segments: Vec::new(),
            checkouts: Vec::new(),
        }
    }

    pub(crate) fn new(
        generation: Generation,
        created_unix_ms: u64,
        next_segment: SegmentId,
        mut segments: Vec<SegmentEntry>,
        mut checkouts: Vec<StateRef>,
    ) -> Self {
        segments.sort_by_key(|entry| entry.id);
        checkouts.sort_by_key(|state| state.checkout);
        Self {
            generation,
            created_unix_ms,
            next_segment,
            segments,
            checkouts,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn created_unix_ms(&self) -> u64 {
        self.created_unix_ms
    }

    /// The lowest segment id no segment of this family has used.
    #[must_use]
    pub const fn next_segment(&self) -> SegmentId {
        self.next_segment
    }

    /// The segments of this generation, by id.
    #[must_use]
    pub fn segments(&self) -> &[SegmentEntry] {
        &self.segments
    }

    #[must_use]
    pub fn segment(&self, id: SegmentId) -> Option<&SegmentEntry> {
        self.segments
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
            .map(|index| &self.segments[index])
    }

    /// Every checkout's current state at this generation, by checkout.
    #[must_use]
    pub fn checkouts(&self) -> &[StateRef] {
        &self.checkouts
    }

    #[must_use]
    pub fn checkout(&self, checkout: CheckoutId) -> Option<&StateRef> {
        self.checkouts
            .binary_search_by_key(&checkout, |state| state.checkout)
            .ok()
            .map(|index| &self.checkouts[index])
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(28 + 16 * self.segments.len() + 48 * self.checkouts.len());
        out.extend_from_slice(&self.created_unix_ms.to_le_bytes());
        out.extend_from_slice(&self.next_segment.get().to_le_bytes());
        out.extend_from_slice(&count(self.segments.len()).to_le_bytes());
        for entry in &self.segments {
            out.extend_from_slice(&entry.id.get().to_le_bytes());
            out.extend_from_slice(&entry.len.to_le_bytes());
        }
        out.extend_from_slice(&count(self.checkouts.len()).to_le_bytes());
        for state in &self.checkouts {
            out.extend_from_slice(state.checkout.digest().as_bytes());
            out.extend_from_slice(&state.number.to_le_bytes());
            out.extend_from_slice(&state.len.to_le_bytes());
            out.extend_from_slice(state.digest.as_bytes());
        }
        out
    }

    pub(crate) fn decode(generation: Generation, payload: &[u8]) -> Result<Self, RecordError> {
        let mut input = Payload::new(payload);
        let created_unix_ms = input.u64()?;
        let next_segment = SegmentId::new(input.u64()?);
        let segment_count = input.u32()?;
        let mut segments = Vec::new();
        for _ in 0..segment_count {
            let id = SegmentId::new(input.u64()?);
            let len = input.u64()?;
            if segments
                .last()
                .is_some_and(|last: &SegmentEntry| last.id >= id)
                || id >= next_segment
            {
                return Err(RecordError::Invalid(
                    "segments out of order or beyond the next id",
                ));
            }
            segments.push(SegmentEntry { id, len });
        }
        let checkout_count = input.u32()?;
        let mut checkouts = Vec::new();
        for _ in 0..checkout_count {
            let checkout = CheckoutId::from_digest(input.digest()?);
            let number = input.u64()?;
            let len = input.u64()?;
            let digest = input.digest()?;
            if checkouts
                .last()
                .is_some_and(|last: &StateRef| last.checkout >= checkout)
            {
                return Err(RecordError::Invalid("checkouts out of order"));
            }
            checkouts.push(StateRef {
                checkout,
                number,
                len,
                digest,
            });
        }
        input.finish()?;
        Ok(Self {
            generation,
            created_unix_ms,
            next_segment,
            segments,
            checkouts,
        })
    }
}

fn count(len: usize) -> u32 {
    u32::try_from(len).expect("a manifest names fewer than 4 billion entries")
}

crate::heap_bytes_none!(SegmentEntry, StateRef);
crate::heap_bytes_of_fields!(Manifest {
    generation,
    created_unix_ms,
    next_segment,
    segments,
    checkouts,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_sorted() {
        let checkout = |byte| CheckoutId::from_digest(Digest128::from_bytes([byte; 16]));
        let manifest = Manifest::new(
            Generation::new(4),
            1_793_700_000_000,
            SegmentId::new(10),
            vec![
                SegmentEntry {
                    id: SegmentId::new(7),
                    len: 300,
                },
                SegmentEntry {
                    id: SegmentId::new(2),
                    len: 100,
                },
            ],
            vec![StateRef {
                checkout: checkout(9),
                number: 3,
                len: 12,
                digest: Digest128::of(b"state"),
            }],
        );
        assert_eq!(manifest.segments()[0].id, SegmentId::new(2));
        let decoded = Manifest::decode(Generation::new(4), &manifest.encode()).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.segment(SegmentId::new(7)).unwrap().len, 300);
        assert!(decoded.segment(SegmentId::new(3)).is_none());
        assert_eq!(decoded.checkout(checkout(9)).unwrap().number, 3);
    }

    #[test]
    fn refuses_ids_at_or_beyond_the_next_id() {
        let manifest = Manifest::new(
            Generation::new(1),
            0,
            SegmentId::new(5),
            vec![SegmentEntry {
                id: SegmentId::new(5),
                len: 1,
            }],
            Vec::new(),
        );
        assert!(Manifest::decode(Generation::new(1), &manifest.encode()).is_err());
    }
}
