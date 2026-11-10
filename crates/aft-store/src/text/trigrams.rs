//! Trigrams of file contents and of query strings.
//!
//! A trigram is three consecutive bytes with ASCII letters folded to lower
//! case, packed into the low 24 bits of a `u32`. Each trigram of a content
//! carries a next-character mask: one bit, chosen by a small hash of the
//! folded byte that follows, for every place the trigram occurs. A query
//! that needs `abc` followed by `d` skips a content whose `abc` mask lacks
//! `d`'s bit. Masks only ever skip contents that cannot match; a shared bit
//! just lets a non-match through to verification.

/// Folds one byte the way the index does: ASCII letters to lower case,
/// everything else, including bytes of non-ASCII characters, unchanged.
#[must_use]
pub const fn fold(byte: u8) -> u8 {
    byte.to_ascii_lowercase()
}

/// Packs three already-folded bytes into a trigram.
#[must_use]
pub const fn pack(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | c as u32
}

/// The mask bit of a byte that follows a trigram.
#[must_use]
pub const fn next_bit(byte: u8) -> u8 {
    1 << (fold(byte).wrapping_mul(31) & 7)
}

/// Every distinct trigram of one content, in ascending order, each with the
/// mask of the bytes that follow it. A trigram that occurs only at the end of
/// the content has an empty mask.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ContentTrigrams {
    entries: Vec<(u32, u8)>,
}

impl ContentTrigrams {
    /// The trigrams of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        if bytes.len() < 3 {
            return Self::default();
        }
        let mut all = Vec::with_capacity(bytes.len() - 2);
        for start in 0..=bytes.len() - 3 {
            let trigram = pack(
                fold(bytes[start]),
                fold(bytes[start + 1]),
                fold(bytes[start + 2]),
            );
            let mask = bytes.get(start + 3).map_or(0, |next| next_bit(*next));
            all.push((trigram, mask));
        }
        all.sort_unstable_by_key(|(trigram, _)| *trigram);
        let mut entries: Vec<(u32, u8)> = Vec::new();
        for (trigram, mask) in all {
            match entries.last_mut() {
                Some(last) if last.0 == trigram => last.1 |= mask,
                _ => entries.push((trigram, mask)),
            }
        }
        entries.shrink_to_fit();
        Self { entries }
    }

    /// Trigrams and masks in ascending trigram order.
    #[must_use]
    pub fn entries(&self) -> &[(u32, u8)] {
        &self.entries
    }

    /// How many distinct trigrams the content has.
    #[must_use]
    pub fn distinct(&self) -> usize {
        self.entries.len()
    }

    /// The mask of `trigram`, if the content has it.
    #[must_use]
    pub fn get(&self, trigram: u32) -> Option<u8> {
        self.entries
            .binary_search_by_key(&trigram, |(key, _)| *key)
            .ok()
            .map(|index| self.entries[index].1)
    }
}

crate::heap_bytes_of_fields!(ContentTrigrams { entries });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_and_merges_masks() {
        let trigrams = ContentTrigrams::of(b"FooX fooy foo");
        let foo = pack(b'f', b'o', b'o');
        // "Foo" then "X", "foo" then "y", and "foo" at the end adds nothing.
        assert_eq!(trigrams.get(foo), Some(next_bit(b'x') | next_bit(b'y')));
        assert!(
            trigrams
                .entries()
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
        );
        assert_eq!(ContentTrigrams::of(b"ab").distinct(), 0);
    }

    #[test]
    fn non_ascii_bytes_are_not_folded() {
        let upper = ContentTrigrams::of("É".repeat(2).as_bytes());
        let lower = ContentTrigrams::of("é".repeat(2).as_bytes());
        assert_ne!(upper, lower);
    }
}
