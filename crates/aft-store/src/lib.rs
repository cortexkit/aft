//! On-disk formats for AFT's indexes.
//!
//! Everything AFT stores about a repository's files lives in immutable
//! segment files: units keyed by file content, sorted lookup tables, and
//! posting lists. Readers fetch segment bytes with positioned reads through
//! one byte-budgeted [`BlockCache`] shared by every open segment, so memory
//! stays bounded and counted however many checkouts read the same data.
//!
//! This crate knows nothing about AFT's tools, sessions or configuration. It
//! defines the formats and the code that writes, verifies and reads them.

pub mod cache;
pub mod checkout;
pub mod content;
pub mod family;
pub mod graph;
pub mod heap;
pub mod ids;
pub mod postings;
pub mod scan;
pub mod segment;
pub mod stat;
pub mod table;
pub mod text;
pub mod vector;

mod fault;
mod fs;
mod le;
mod packed;
#[cfg(test)]
mod test_rng;

pub use cache::{BlockCache, CacheStats};
pub use family::{Family, FamilyError, Snapshot, WriteSession};
pub use ids::{
    CheckoutId, ContentKey, Digest128, FamilyId, Generation, SegmentId, WriterIncarnation,
};
