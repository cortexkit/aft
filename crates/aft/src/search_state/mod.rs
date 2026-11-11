//! Search-owned state and request primitives shared with the runtime.

pub mod candidate;
pub mod evidence_descriptor;
pub mod generation_token;
pub mod memo;
pub mod request_files;
pub(crate) mod rerank;
pub mod waits;

pub const BLOCK_DEPTHS: [usize; 5] = [200, 400, 800, 1_600, 3_200];
