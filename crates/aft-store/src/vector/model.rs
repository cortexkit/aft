//! Which model produced a vector, and the keys vectors and chunk maps are
//! stored under.

use std::fmt;

use crate::ids::{ContentKey, Digest128};
use crate::le::{u16_at, u32_at};

/// Everything that decides what vector an embedder returns for a text.
///
/// Two identities that differ in any field never share vectors: a new model
/// revision, another dimension or a changed text template means every
/// vector must be embedded again.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ModelIdentity {
    /// The model's name as its backend reports it, such as
    /// `sentence-transformers/all-MiniLM-L6-v2`.
    pub name: String,
    /// The model's revision or weights digest; empty when the backend has
    /// none.
    pub revision: String,
    pub dimensions: u32,
    /// Whether the backend returns vectors scaled to unit length. Scoring is
    /// cosine either way; a backend that changes this returns different
    /// vectors.
    pub normalized: bool,
    /// Version of the template that builds the text sent to the embedder.
    pub text_template: u32,
    /// Version of the rules that cut a file into chunks. Chunk maps depend
    /// on it; vectors depend only on the text they were given.
    pub chunk_template: u32,
}

/// Fingerprints are refused when their strings are longer than this, so a
/// damaged length cannot ask for an absurd allocation.
const MAX_IDENTITY_STRING: usize = 4096;

impl ModelIdentity {
    /// The identity as stored in a segment.
    ///
    /// # Panics
    ///
    /// When the name or revision is longer than 4,096 bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            self.name.len() <= MAX_IDENTITY_STRING && self.revision.len() <= MAX_IDENTITY_STRING,
            "model name and revision must be at most {MAX_IDENTITY_STRING} bytes"
        );
        let mut out = Vec::with_capacity(17 + self.name.len() + self.revision.len());
        out.extend_from_slice(&self.dimensions.to_le_bytes());
        out.push(u8::from(self.normalized));
        out.extend_from_slice(&self.text_template.to_le_bytes());
        out.extend_from_slice(&self.chunk_template.to_le_bytes());
        for text in [&self.name, &self.revision] {
            let len = u16::try_from(text.len()).expect("checked above");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        out
    }

    /// Reads an identity back; `None` when the bytes are not one.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 13 {
            return None;
        }
        let normalized = match bytes[4] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let mut at = 13;
        let mut strings = Vec::with_capacity(2);
        for _ in 0..2 {
            if bytes.len() < at + 2 {
                return None;
            }
            let len = usize::from(u16_at(bytes, at));
            at += 2;
            if len > MAX_IDENTITY_STRING || bytes.len() < at + len {
                return None;
            }
            strings.push(String::from_utf8(bytes[at..at + len].to_vec()).ok()?);
            at += len;
        }
        if at != bytes.len() {
            return None;
        }
        let revision = strings.pop()?;
        let name = strings.pop()?;
        Some(Self {
            name,
            revision,
            dimensions: u32_at(bytes, 0),
            normalized,
            text_template: u32_at(bytes, 5),
            chunk_template: u32_at(bytes, 9),
        })
    }

    /// The identity's digest, the model half of every vector key.
    #[must_use]
    pub fn id(&self) -> ModelId {
        let mut hasher = blake3::Hasher::new_derive_key("aft-store 2026-11 vector model identity");
        hasher.update(&self.encode());
        ModelId(Digest128::from_hash(&hasher.finalize()))
    }
}

/// The digest of a [`ModelIdentity`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId(Digest128);

impl ModelId {
    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModelId({})", self.0)
    }
}

/// The digest of the exact bytes sent to the embedder for one chunk. With
/// the model's identity it is the key of one vector.
///
/// The embedded text includes the file's path, so the same symbol at two
/// paths has two keys; the same path and text in any checkout of the family
/// shares one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextDigest(Digest128);

impl TextDigest {
    /// The key of the embedded text `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Digest128::of(bytes))
    }

    /// A digest read back from storage.
    #[must_use]
    pub const fn from_digest(digest: Digest128) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

impl fmt::Display for TextDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for TextDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TextDigest({})", self.0)
    }
}

/// The key of one file's chunk map: the file's content and the path it is
/// at. The path is part of the key because the embedded text includes it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ChunkMapKey {
    content: ContentKey,
    path: Digest128,
}

impl ChunkMapKey {
    pub(crate) const LEN: usize = 2 * Digest128::LEN;

    #[must_use]
    pub fn new(content: ContentKey, path: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("aft-store 2026-11 chunk map path");
        hasher.update(path);
        Self {
            content,
            path: Digest128::from_hash(&hasher.finalize()),
        }
    }

    #[must_use]
    pub const fn content(&self) -> ContentKey {
        self.content
    }

    pub(crate) fn to_bytes(self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..Digest128::LEN].copy_from_slice(self.content.digest().as_bytes());
        out[Digest128::LEN..].copy_from_slice(self.path.as_bytes());
        out
    }

    /// A key read back from a chunk map table; `None` when the bytes are
    /// not one.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN {
            return None;
        }
        Some(Self {
            content: ContentKey::from_digest(Digest128::from_bytes(crate::le::array(bytes, 0))),
            path: Digest128::from_bytes(crate::le::array(bytes, Digest128::LEN)),
        })
    }
}

crate::heap_bytes_none!(ModelId, TextDigest, ChunkMapKey);
crate::heap_bytes_of_fields!(ModelIdentity {
    name,
    revision,
    dimensions,
    normalized,
    text_template,
    chunk_template,
});
