//! Pinned hashline oracle parity artifacts.
//!
//! This module is intentionally self-contained.  The corpus and vectors are
//! generated from `packages/hashline` at the revision below, while the Rust
//! implementation has no dependency on the AFT crate graph.

pub const ORACLE_REVISION: &str = "45e12e5bb758198a920c6070e7e64cb33b21beac";
pub const ORACLE_PACKAGE: &str = "packages/hashline";
pub const XXHASH32_SEED: u32 = 0;

pub mod xxhash32;

pub use xxhash32::{normalize_for_tag, tag_for, xxhash32, xxhash32_seed_zero};
