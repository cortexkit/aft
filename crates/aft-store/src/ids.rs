//! Identities that appear in stored data.

use std::fmt;

/// A BLAKE3 digest truncated to its first 128 bits.
///
/// The store uses it for content keys and for every checksum. 128 bits keep
/// accidental collisions out of reach for any repository, at half the size of
/// the full digest in keys and checksum tables.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest128([u8; 16]);

impl Digest128 {
    /// Length of a digest in bytes.
    pub const LEN: usize = 16;

    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self::from_hash(&blake3::hash(bytes))
    }

    /// The first 128 bits of a full BLAKE3 hash.
    #[must_use]
    pub fn from_hash(hash: &blake3::Hash) -> Self {
        let mut bytes = [0; Self::LEN];
        bytes.copy_from_slice(&hash.as_bytes()[..Self::LEN]);
        Self(bytes)
    }

    /// A digest read back from storage.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl fmt::Display for Digest128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Digest128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest128({self})")
    }
}

/// A repository family: every checkout, clone and fork of one repository on
/// this machine. Members of a family share stored data.
///
/// The value is derived from the repository's root commits by the caller;
/// this type only carries it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FamilyId(Digest128);

impl FamilyId {
    #[must_use]
    pub const fn from_digest(digest: Digest128) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

impl fmt::Display for FamilyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for FamilyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FamilyId({})", self.0)
    }
}

/// The key of a file content: the digest of its bytes as read from the
/// working tree. Units derived from a content are keyed by it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentKey(Digest128);

impl ContentKey {
    /// The key of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Digest128::of(bytes))
    }

    /// A key read back from storage.
    #[must_use]
    pub const fn from_digest(digest: Digest128) -> Self {
        Self(digest)
    }

    /// A key for a file whose bytes are not read, such as one over the size
    /// limit, derived from its path and stat evidence. It is hashed in its
    /// own key-derivation context, so it never equals the key of real bytes,
    /// and it changes whenever the evidence does.
    #[must_use]
    pub fn stand_in(path: &[u8], evidence: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("aft-store 2026-11 unread file stand-in");
        hasher.update(&u64::try_from(path.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(path);
        hasher.update(evidence);
        Self(Digest128::from_hash(&hasher.finalize()))
    }

    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

impl fmt::Display for ContentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for ContentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentKey({})", self.0)
    }
}

/// A segment's id within its family. Ids only increase and are never reused,
/// so a name that once held a segment never holds different bytes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegmentId(u64);

impl SegmentId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One checkout of a family: a main checkout, a worktree or a clone. The value
/// is derived from the checkout's root path and Git directory by the caller.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckoutId(Digest128);

impl CheckoutId {
    #[must_use]
    pub const fn from_digest(digest: Digest128) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

impl fmt::Display for CheckoutId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for CheckoutId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CheckoutId({})", self.0)
    }
}

/// The number of one published manifest. Each publication's generation is one
/// above the newest before it; a generation names one manifest forever.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Generation(u64);

impl Generation {
    /// The generation of a family that has never published: no segments and
    /// no checkouts. No manifest file carries it.
    pub const EMPTY: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The generation published after this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One run of a writing process. Recorded in every segment it writes, so
/// files left behind by a crashed writer can be told apart from a live one's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct WriterIncarnation(u64);

impl WriterIncarnation {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_the_first_half_of_blake3() {
        let full = blake3::hash(b"fn main() {}\n");
        let digest = Digest128::of(b"fn main() {}\n");
        assert_eq!(digest.as_bytes()[..], full.as_bytes()[..16]);
        assert_eq!(digest.to_string(), full.to_hex()[..32]);
    }

    #[test]
    fn content_keys_follow_bytes() {
        assert_eq!(ContentKey::of(b"a"), ContentKey::of(b"a"));
        assert_ne!(ContentKey::of(b"a"), ContentKey::of(b"a\n"));
    }
}
