//! Secret-use rules shared by AFT and basal.
//!
//! An agent writes `{{ck-secret:<label>}}` instead of a key. The vault maps a
//! label to a rule (host, port, path prefix, location); this crate decides
//! whether a request may carry the key, where it goes, what a key may be, how
//! to remove it from responses, and what an audit record may say.
//!
//! The crate is pure: it performs no I/O and holds key material only for the
//! duration of a call. The committed vectors under `vectors/` are the
//! conformance artifact.

pub mod audit;
pub mod location;
pub mod material;
pub mod placeholder;
pub mod redirect;
pub mod scrub;
pub mod target;
mod work;

pub use audit::{
    AuditReason, AuditRecordV1, AuditRefusal, Decision, MatchedPrefix, ScopeRef, AUDIT_VERSION_V1,
};
pub use location::{
    apply_target_edits, evaluate_request_with_connect, is_json_content_type, location_kind_token,
    parse_vault_location, Edit, HeaderEdit, HeaderEditKind, HeaderField, HeaderValueForm,
    LabelOutcome, LabelRule, Location, RequestOutcome, RequestRefusal, RequestView,
};
pub use material::{
    check_admissible, encode_for_location, key_contains_placeholder_prefix,
    key_is_substring_of_placeholder, key_overlaps_placeholder, MaterialRefusal, MIN_KEY_LEN,
};
pub use placeholder::{
    contains_placeholder_prefix, contains_placeholder_prefix_loose, is_label, mentions_label,
    mentions_label_loose, parse_at, parse_exact, scan, Placeholder, PLACEHOLDER_PREFIX,
};
pub use redirect::{
    evaluate_redirect, location_mentions, resolve_location, split_absolute, RedirectOutcome,
};
pub use scrub::{
    reveal_outcome, reveals_any, reveals_key, scrub_bytes, KeyPatterns, MatcherSet, RevealOutcome,
    ScrubError, Scrubber, MAX_REVEAL_DEPTH,
};
pub use target::{
    authority_matches_connect, canonical_host, canonicalize_path, normalize_path_prefix,
    normalize_request_target, path_covers, ConnectAuthority, NormalizedPath, RequestTarget,
    RuleTarget, TargetRefusal,
};
