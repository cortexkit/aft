//! The text plane: a trigram index over every distinct file content of a
//! family, shared by all of its checkouts.
//!
//! Contents are indexed once, keyed by their bytes. A checkout's state says
//! which content each of its paths holds, so a query runs once over the
//! family's postings and keeps the contents that the checkout's paths in
//! scope hold. Nothing is copied per checkout.

pub mod plan;
mod search;
mod sections;
mod trigrams;

pub use plan::{Query, plan, plan_pattern};
pub use search::{
    Located, OverlayEntry, PathText, Scope, TextLocator, TextMembers, TextMembership, TextOverlay,
    candidates, path_text, paths_with_trigram, paths_with_trigram_among, trigram_frequency,
};
pub use sections::{KNOWN, TextSections, TextSectionsBuilder, TextSegment, TextStatus, TextUnit};
pub use trigrams::{ContentTrigrams, fold, next_bit, pack};
