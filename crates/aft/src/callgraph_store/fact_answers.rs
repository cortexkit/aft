pub use aft_parse::callgraph_store::fact_answers::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::facts::{EntryKind, ProjectFacts};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeMap;

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
