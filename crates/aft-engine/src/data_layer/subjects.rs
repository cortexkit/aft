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
//! | is a folder | whether the folder exists (holds any path) |
//! | folder listing | the folder's direct entries: their names and kinds |
//! | path after following symlinks | each prefix it checks for a symlink, whether each folder it steps into exists, the path it ends at |
//! | configuration bytes or field | what following the path's symlinks consults, and the file it ends at |
//!
//! A folder's existence and its listing are separate subjects because
//! nearly every resolution steps into the folders its references name,
//! while few list them: adding a file to a folder changes its listing, not
//! whether it exists, so it must not reach every read that stepped in.

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
    /// Whether a folder exists: whether any path is under it.
    Dir(Vec<u8>),
    /// A folder's direct entries: their names and kinds.
    Listing(Vec<u8>),
}

impl Subject {
    /// The stored term this subject is filed under.
    pub(crate) fn term(&self) -> Vec<u8> {
        let (tag, path) = match self {
            Self::Path(path) => (b'p', path),
            Self::Dir(path) => (b'd', path),
            Self::Listing(path) => (b'l', path),
        };
        let mut term = Vec::with_capacity(path.len() + 2);
        term.push(tag);
        term.push(0);
        term.extend_from_slice(path);
        term
    }
}

/// The subjects that changed when `changed` went from its record in
/// `before` to its record in `after`: always the path itself (its bytes may
/// have changed). When the path appeared, went away, or changed kind or
/// link target, also its folder's listing, and for each folder above it
/// that came to exist or stopped existing, that folder and its own folder's
/// listing. A folder that existed before and after keeps existing, and so do
/// the folders above it.
pub(crate) fn affected_by(
    changed: &[u8],
    before: &CheckoutFacts<'_>,
    after: &CheckoutFacts<'_>,
) -> Vec<Subject> {
    let mut subjects = vec![Subject::Path(changed.to_vec())];
    let record = |facts: &CheckoutFacts<'_>| {
        facts
            .inputs
            .state
            .get(changed)
            .map(|record| (record.kind, record.link.clone()))
    };
    if record(before) == record(after) {
        return subjects;
    }
    let parent = |path: &[u8]| -> Vec<u8> {
        path.iter()
            .rposition(|b| *b == b'/')
            .map_or_else(Vec::new, |slash| path[..slash].to_vec())
    };
    let mut folder = parent(changed);
    subjects.push(Subject::Listing(folder.clone()));
    loop {
        if before.is_dir(&folder) == after.is_dir(&folder) {
            break;
        }
        subjects.push(Subject::Dir(folder.clone()));
        if folder.is_empty() {
            break;
        }
        folder = parent(&folder);
        subjects.push(Subject::Listing(folder.clone()));
    }
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
                note(Subject::Dir(folder.to_vec()));
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
        FactQuestion::IsDir(path) => note(Subject::Dir(path.clone())),
        FactQuestion::ListDir(path) => note(Subject::Listing(path.clone())),
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
    use std::collections::HashMap;

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
            parses: crate::data_layer::callgraph::Parses::default(),
            configs: HashMap::new(),
        }
    }

    /// Whether the change to `changed` reaches a subject of `question`, by
    /// [`affected_by`].
    fn reached(
        question: &FactQuestion,
        before: &CheckoutInputs,
        after: &CheckoutInputs,
        changed: &str,
    ) -> bool {
        let consulted = subjects(question, &CheckoutFacts { inputs: before });
        affected_by(
            changed.as_bytes(),
            &CheckoutFacts { inputs: before },
            &CheckoutFacts { inputs: after },
        )
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
        let mut reached_unmoved = Vec::new();
        for (changed, apply) in &changes {
            let mut records = base.clone();
            apply(&mut records);
            let after = inputs(records);
            for question in &questions {
                let old = question.answer(&CheckoutFacts { inputs: &before });
                let new = question.answer(&CheckoutFacts { inputs: &after });
                let reached = reached(question, &before, &after, changed);
                if old != new {
                    moved += 1;
                    assert!(
                        reached,
                        "{question:?} changed after {changed} but no subject was reached"
                    );
                } else if reached {
                    reached_unmoved.push(format!("{changed}: {question:?}"));
                }
            }
        }
        // The fixture must move answers, or the check proves nothing.
        assert!(moved >= 7, "only {moved} answers moved");
        // And the subjects are narrow: adding a file to a folder that exists
        // reaches neither a read that only stepped into the folder nor
        // whether the folder exists.
        let stepped_in = reached_unmoved
            .iter()
            .filter(|line| {
                line.starts_with("crates/a/src/new.rs:") && line.contains("Canonical")
                    || line.starts_with("crates/a/src/new.rs:") && line.contains("IsDir")
            })
            .collect::<Vec<_>>();
        assert!(stepped_in.is_empty(), "{stepped_in:#?}");
    }

    #[test]
    fn a_content_change_reaches_only_the_path() {
        let base = vec![
            record("Cargo.toml", PathKind::Text, ""),
            record("crates/a/src/lib.rs", PathKind::Text, ""),
        ];
        let before = inputs(base.clone());
        let mut edited = base;
        edited[0].content = ContentKey::of(b"edited");
        let after = inputs(edited);
        assert_eq!(
            affected_by(
                b"Cargo.toml",
                &CheckoutFacts { inputs: &before },
                &CheckoutFacts { inputs: &after }
            ),
            vec![Subject::Path(b"Cargo.toml".to_vec())]
        );
    }

    #[test]
    fn a_new_folder_reaches_its_existence_and_each_listing_above_it() {
        let base = vec![record("crates/a/src/lib.rs", PathKind::Text, "")];
        let before = inputs(base.clone());
        let mut added = base;
        added.push(record("crates/b/src/lib.rs", PathKind::Text, ""));
        let after = inputs(added);
        let subjects = affected_by(
            b"crates/b/src/lib.rs",
            &CheckoutFacts { inputs: &before },
            &CheckoutFacts { inputs: &after },
        );
        assert_eq!(
            subjects,
            vec![
                Subject::Path(b"crates/b/src/lib.rs".to_vec()),
                Subject::Listing(b"crates/b/src".to_vec()),
                Subject::Dir(b"crates/b/src".to_vec()),
                Subject::Listing(b"crates/b".to_vec()),
                Subject::Dir(b"crates/b".to_vec()),
                Subject::Listing(b"crates".to_vec()),
            ]
        );
    }
}
