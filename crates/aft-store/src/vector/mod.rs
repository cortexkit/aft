//! The vector plane: embeddings for meaning search, stored once per family
//! and searched exactly.
//!
//! A vector is keyed by the model that made it and the exact text it was
//! made from ([`TextDigest`]); that text includes the file's path, so
//! vectors are per (path, chunk text), and every checkout holding the same
//! file at the same path shares them. A chunk map per (content, path) lists
//! a file's chunks in order, each by the text it was embedded from.
//!
//! Each vector is stored twice: as int8 codes with the values that bound
//! their error, which every search scans, and as the f32 vector the
//! embedder returned, read only for the few members that could still reach
//! the top results. The answer equals today's f32 brute-force answer: the
//! same members, in the same order, with the same score bits (see
//! [`VectorIndex::search`]).
//!
//! The codes are per-vector symmetric int8 of each vector's offset from its
//! segment's mean vector (`cᵢ = round((vᵢ − μᵢ) / s)`, with
//! `s = max |vᵢ − μᵢ| / 127`). Each vector also stores its residual norm
//! and its largest component residual, so the bound needs nothing else.
//! Centering was chosen over plain per-vector and per-dimension scaling by
//! measurement (`examples/vector_gate.rs`): it rescored the fewest members on
//! every model measured.

mod model;
mod score;
mod search;
mod sections;

#[cfg(test)]
mod tests;

pub use model::{ChunkMapKey, ModelId, ModelIdentity, TextDigest};
pub use score::{dot_product, rank_order, score, vector_norm};
pub use search::{
    Coverage, FileMembers, FullScan, Hit, MappedChunk, Member, MemberSet, ScoreBounds,
    SearchReport, SearchRequest, SearchResult, SharedVectorIndex, UnitRef, VectorIndex,
    default_rescore_limit,
};
pub use sections::{
    Chunk, ChunkDetail, FileDetails, KNOWN, MAX_DIMENSIONS, SectionBytes, VectorSectionsBuilder,
    VectorSegment, VectorSegmentParts,
};

use crate::segment::SegmentError;

/// Why a vector operation failed.
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// The family's segments hold no vector of this model: meaning search
    /// has nothing to search yet, which is not the same as finding nothing.
    #[error("no vectors are stored for model {0}")]
    NoVectors(ModelId),
    #[error("the model has {expected} dimensions; the vector has {found}")]
    DimensionMismatch { expected: u32, found: usize },
    #[error("a model with {0} dimensions is not supported")]
    UnsupportedDimensions(u32),
    #[error("a vector has a component that is not finite, or a norm that overflows")]
    NonFinite,
    #[error("a member names vector {} of segment {}, which the index does not have", .0.ordinal, .0.segment)]
    UnknownUnit(UnitRef),
    #[error("the member set was prepared for another vector index")]
    ForeignMembers,
    #[error("more members than a search can rank")]
    TooManyMembers,
    #[error("more segments than an index can hold")]
    TooManySegments,
    /// Details given with a chunk map do not describe its chunks.
    #[error("chunk details do not describe the chunk map's chunks")]
    DetailsMismatch,
}
