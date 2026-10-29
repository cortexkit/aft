//! The published manifest for AFT's rich UI (`ckios.richui.manifest/1`).
//!
//! The phone's rich UI draws tool pages from this manifest: it fetches the
//! canonical bytes by digest, verifies the digest, and renders templates that
//! read AFT through a carrier. The pieces live here in separate files:
//!
//! - `canonical`: RFC 8785 (JCS) canonicalization, so the digest the phone
//!   computes over the bytes AFT publishes equals the one AFT computes.
//! - `schema`: the bounded schema language of design 2.2, as a checker for the
//!   published schemas and for replies projected into them.
//! - `jcs_number`: the number form JCS serializes with (ECMAScript
//!   `Number::toString`), which differs from Rust's `f64` formatting.
//!
//! - `operations`: versioned schemas, closed projections and safe messages.
//! - `builder`: deterministic assembly and caps, with no production manifest
//!   until the publication prerequisites are reviewed.
//! - `chunks`: the `rich_ui.manifest_chunk` and `rich_ui.template_chunk`
//!   provider reads that serve manifest and template bytes to Occipital.
//! - `session`: stamped session roles, policy-route selection and read dispatch.
//!
//! Operation admission is live but fails closed without a published manifest.
//! Session reads, command views and bounded polling watches use the selected
//! policy route without inheriting worker wait or task-lifetime side effects.

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod admission;
mod bash_output;
mod bash_watch;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod builder;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod canonical;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod carrier;
pub(crate) mod chunks;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod jcs_number;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod operations;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod schema;
pub(super) mod session;
#[cfg(test)]
mod vectors;
