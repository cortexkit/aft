//! Path and configuration facts a resolution consulted, with the answers it
//! got.
//!
//! The resolver reads a checkout only through [`ProjectFacts`]. When
//! answers are recorded, every such read is kept as a typed question with
//! its answer. A stored resolution is then valid in another checkout, or
//! after an edit, exactly when every recorded question still has its
//! recorded answer there; no reasoning about which change could affect which
//! read is needed. Surface queries (what other files export) are recorded
//! separately, with their answers, by the surface index.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::facts::{EntryKind, ProjectFacts};

/// One read of the checkout, by the path it named.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum FactQuestion {
    IsFile(Vec<u8>),
    IsDir(Vec<u8>),
    /// The target of a symlink, or none for a path that is not one.
    Link(Vec<u8>),
    /// The path after following symlinks inside the checkout.
    Canonical(Vec<u8>),
    /// A directory's entries and their kinds.
    ListDir(Vec<u8>),
    /// A configuration file's bytes, read without naming a field.
    Config(Vec<u8>),
    /// One field of a configuration file, as the resolver names it (see
    /// [`config_field_answer`]).
    ConfigField(Vec<u8>, String),
}

/// What a [`FactQuestion`] was answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FactAnswer {
    Bool(bool),
    Path(Option<Vec<u8>>),
    /// A digest of a listing or of file bytes; listings and files can be
    /// large, and only equality matters.
    Digest([u8; 16]),
}

/// Recorded questions and their answers.
pub(crate) type FactAnswers = BTreeMap<FactQuestion, FactAnswer>;

impl FactQuestion {
    /// Asks the question again of `facts`.
    pub(crate) fn answer(&self, facts: &dyn ProjectFacts) -> FactAnswer {
        match self {
            Self::IsFile(path) => FactAnswer::Bool(facts.is_file(path)),
            Self::IsDir(path) => FactAnswer::Bool(facts.is_dir(path)),
            Self::Link(path) => FactAnswer::Path(facts.symlink_target(path).map(<[u8]>::to_vec)),
            Self::Canonical(path) => FactAnswer::Path(facts.canonical(path)),
            Self::ListDir(path) => listing_answer(&facts.list_dir(path)),
            Self::Config(path) => config_answer(facts.config_bytes(path).as_deref()),
            Self::ConfigField(path, name) => {
                config_field_answer(path, facts.config_bytes(path).as_deref(), name)
            }
        }
    }
}

/// The answer for a directory listing.
pub(crate) fn listing_answer(entries: &[super::facts::DirEntry]) -> FactAnswer {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aft listing 1");
    for entry in entries {
        hasher.update(&(entry.name.len() as u64).to_le_bytes());
        hasher.update(&entry.name);
        hasher.update(&[match entry.kind {
            EntryKind::Regular => 0,
            EntryKind::Directory => 1,
            EntryKind::Symlink => 2,
        }]);
    }
    FactAnswer::Digest(truncate(&hasher.finalize()))
}

/// The answer for a configuration file's bytes, or for its absence.
pub(crate) fn config_answer(bytes: Option<&[u8]>) -> FactAnswer {
    let mut hasher = blake3::Hasher::new();
    match bytes {
        Some(bytes) => {
            hasher.update(b"aft config 1 present");
            hasher.update(bytes);
        }
        None => {
            hasher.update(b"aft config 1 absent");
        }
    }
    FactAnswer::Digest(truncate(&hasher.finalize()))
}

/// The answer for one field of a configuration file: the digest of the
/// field's value as the resolver reads it, the same projection today's
/// refresh compares. A field the projection does not know, or a file it
/// cannot project, falls back to the whole file's answer: never finer than
/// what is known to be the field's only input.
pub(crate) fn config_field_answer(path: &[u8], bytes: Option<&[u8]>, name: &str) -> FactAnswer {
    let projected = bytes
        .and_then(|bytes| super::resolution_facts::project(path, bytes))
        .and_then(|fields| fields.get(name).cloned());
    match projected {
        Some(digest) => {
            let hash = blake3::hash(digest.as_bytes());
            FactAnswer::Digest(truncate(&hash))
        }
        None if bytes.is_none() => config_answer(None),
        None => config_answer(bytes),
    }
}

fn truncate(hash: &blake3::Hash) -> [u8; 16] {
    let mut digest = [0; 16];
    digest.copy_from_slice(&hash.as_bytes()[..16]);
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_tell_contents_and_absence_apart() {
        assert_ne!(config_answer(Some(b"")), config_answer(None));
        assert_ne!(config_answer(Some(b"a")), config_answer(Some(b"b")));
        let entry = |name: &[u8], kind| super::super::facts::DirEntry {
            name: name.to_vec(),
            kind,
        };
        // A name boundary moving between entries changes the listing.
        assert_ne!(
            listing_answer(&[
                entry(b"ab", EntryKind::Regular),
                entry(b"c", EntryKind::Regular)
            ]),
            listing_answer(&[
                entry(b"a", EntryKind::Regular),
                entry(b"bc", EntryKind::Regular)
            ])
        );
        assert_ne!(
            listing_answer(&[entry(b"a", EntryKind::Regular)]),
            listing_answer(&[entry(b"a", EntryKind::Directory)])
        );
    }
}
