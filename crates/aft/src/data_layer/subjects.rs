//! The paths and folders a recorded checkout read consulted.
//!
//! A resolution records each checkout read it made with the answer it got
//! (see `callgraph_store::fact_answers`). After an edit, a read can only
//! answer differently if something it consulted changed. So each read is
//! filed under what it consulted, and an edit asks again only the reads
//! filed under a changed path or under a folder holding one, instead of
//! every read of every file.
//!
//! What a read consults, given how [`CheckoutFacts`] answers it:
//!
//! | Read | Consults |
//! |---|---|
//! | is a file, symlink target | the path itself |
//! | is a folder, folder listing | everything under the folder |
//! | path after following symlinks | each prefix it checks for a symlink, each folder it steps into, the path it ends at |
//! | configuration bytes or field | what following the path's symlinks consults, and the file it ends at |

use std::cell::RefCell;
use std::collections::BTreeSet;

use crate::callgraph_store::fact_answers::FactQuestion;
use crate::callgraph_store::facts::ProjectFacts;

use super::callgraph::{canonical_walk, CheckoutFacts};

/// Something a recorded read consulted.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum Subject {
    /// One path's record: whether it exists, its kind, its link target, its
    /// bytes.
    Path(Vec<u8>),
    /// Every path under a folder: which exist and their kinds.
    Folder(Vec<u8>),
}

impl Subject {
    /// The stored term this subject is filed under.
    pub(crate) fn term(&self) -> Vec<u8> {
        let (tag, path) = match self {
            Self::Path(path) => (b'p', path),
            Self::Folder(path) => (b'f', path),
        };
        let mut term = Vec::with_capacity(path.len() + 2);
        term.push(tag);
        term.push(0);
        term.extend_from_slice(path);
        term
    }
}

/// The subjects whose change can change a read's answer, for a change to
/// `changed`: the path itself, and every folder holding it, up to the
/// checkout's root.
pub(crate) fn affected_by(changed: &[u8]) -> Vec<Subject> {
    let mut subjects = vec![
        Subject::Path(changed.to_vec()),
        Subject::Folder(changed.to_vec()),
    ];
    let mut folder = changed;
    while let Some(slash) = folder.iter().rposition(|b| *b == b'/') {
        folder = &folder[..slash];
        subjects.push(Subject::Folder(folder.to_vec()));
    }
    subjects.push(Subject::Folder(Vec::new()));
    subjects
}

/// What `question` consults in the checkout `facts` answers for.
pub(crate) fn subjects(question: &FactQuestion, facts: &CheckoutFacts<'_>) -> BTreeSet<Subject> {
    let consulted = RefCell::new(BTreeSet::new());
    let note = |subject: Subject| {
        consulted.borrow_mut().insert(subject);
    };
    // Follows `path`'s symlinks as `CheckoutFacts::canonical` does, noting
    // everything the walk consults.
    let follow = |path: &[u8]| {
        canonical_walk(
            path,
            |prefix| {
                note(Subject::Path(prefix.to_vec()));
                facts.symlink_target(prefix).map(<[u8]>::to_vec)
            },
            |folder| {
                note(Subject::Folder(folder.to_vec()));
                facts.is_dir(folder)
            },
            |end| {
                note(Subject::Path(end.to_vec()));
                facts.inputs.state.get(end).is_some()
            },
        )
    };
    match question {
        FactQuestion::IsFile(path) | FactQuestion::Link(path) => note(Subject::Path(path.clone())),
        FactQuestion::IsDir(path) | FactQuestion::ListDir(path) => {
            note(Subject::Folder(path.clone()));
        }
        FactQuestion::Canonical(path) => {
            follow(path);
        }
        FactQuestion::Config(path) | FactQuestion::ConfigField(path, _) => {
            if let Some(end) = follow(path) {
                note(Subject::Path(end));
            }
        }
    }
    consulted.into_inner()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use aft_store::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence};
    use aft_store::ids::{ContentKey, Digest128};

    use super::*;
    use crate::data_layer::callgraph::CheckoutInputs;

    fn record(path: &str, kind: PathKind, link: &str) -> PathRecord {
        PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o100_644,
            kind,
            content: ContentKey::of(path.as_bytes()),
            link: link.as_bytes().to_vec(),
            stat: StatEvidence::default(),
            language: String::new(),
        }
    }

    fn inputs(records: Vec<PathRecord>) -> CheckoutInputs {
        CheckoutInputs {
            state: CheckoutState::new(Vec::new(), Digest128::of(b""), records).unwrap(),
            parses: BTreeMap::new(),
            configs: HashMap::new(),
        }
    }

    /// Whether some change in `changed` reaches a subject of `question`, by
    /// [`affected_by`].
    fn reached(question: &FactQuestion, before: &CheckoutInputs, changed: &str) -> bool {
        let consulted = subjects(question, &CheckoutFacts { inputs: before });
        affected_by(changed.as_bytes())
            .iter()
            .any(|subject| consulted.contains(subject))
    }

    #[test]
    fn every_read_whose_answer_changes_is_reached_by_the_change() {
        // A checkout with a symlinked folder, then each kind of change. For
        // every read and change: if the answer differs after the change, the
        // change must reach one of the read's subjects.
        let base = vec![
            record("Cargo.toml", PathKind::Text, ""),
            record("crates/a/src/lib.rs", PathKind::Text, ""),
            record("crates/a/src/util.rs", PathKind::Text, ""),
            record("link", PathKind::Symlink, "crates/a"),
            record("docs/readme.md", PathKind::Text, ""),
        ];
        let questions = [
            FactQuestion::IsFile(b"crates/a/src/lib.rs".to_vec()),
            FactQuestion::IsFile(b"crates/a/src/new.rs".to_vec()),
            FactQuestion::IsDir(b"crates/a".to_vec()),
            FactQuestion::IsDir(b"crates/b".to_vec()),
            FactQuestion::ListDir(b"crates/a/src".to_vec()),
            FactQuestion::ListDir(Vec::new()),
            FactQuestion::Link(b"link".to_vec()),
            FactQuestion::Canonical(b"link/src/lib.rs".to_vec()),
            FactQuestion::Canonical(b"link/src/new.rs".to_vec()),
            FactQuestion::Config(b"link/Cargo.toml".to_vec()),
        ];
        let changes: Vec<(&str, Box<dyn Fn(&mut Vec<PathRecord>)>)> = vec![
            (
                "crates/a/src/new.rs",
                Box::new(|records: &mut Vec<PathRecord>| {
                    records.push(record("crates/a/src/new.rs", PathKind::Text, ""));
                }),
            ),
            (
                "crates/a/src/util.rs",
                Box::new(|records: &mut Vec<PathRecord>| {
                    records.retain(|r| r.path != b"crates/a/src/util.rs");
                }),
            ),
            (
                "link",
                Box::new(|records: &mut Vec<PathRecord>| {
                    for r in records.iter_mut().filter(|r| r.path == b"link") {
                        r.link = b"docs".to_vec();
                    }
                }),
            ),
            (
                "crates/a/Cargo.toml",
                Box::new(|records: &mut Vec<PathRecord>| {
                    records.push(record("crates/a/Cargo.toml", PathKind::Text, ""));
                }),
            ),
            (
                "crates/b/lib.rs",
                Box::new(|records: &mut Vec<PathRecord>| {
                    records.push(record("crates/b/lib.rs", PathKind::Text, ""));
                }),
            ),
        ];
        let before = inputs(base.clone());
        let mut moved = 0;
        for (changed, apply) in &changes {
            let mut records = base.clone();
            apply(&mut records);
            let after = inputs(records);
            for question in &questions {
                let old = question.answer(&CheckoutFacts { inputs: &before });
                let new = question.answer(&CheckoutFacts { inputs: &after });
                if old != new {
                    moved += 1;
                    assert!(
                        reached(question, &before, changed),
                        "{question:?} changed after {changed} but no subject was reached"
                    );
                }
            }
        }
        // The fixture must move answers, or the check proves nothing.
        assert!(moved >= 8, "only {moved} answers moved");
    }
}
