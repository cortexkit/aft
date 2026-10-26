//! P2 and P4: where a key may go in a request, and when to refuse instead of
//! guess.
//!
//! A label's rule comes only from the vault, so [`LabelRule`] is the request
//! authority this crate is allowed to trust. Evaluation is a pure function of a
//! parsed request, the connection's CONNECT authority and a key per label: it
//! returns the byte edits a proxy applies, or a refusal.
//!
//! Each key belongs to exactly one label. A request naming label A never
//! receives B's key, and a label with no key refuses the request rather than
//! borrowing another one. Edits are collected before anything is applied, and
//! any refusal empties them all, which is what makes substitution atomic per
//! request.

mod json;

pub use json::is_json_content_type;

use crate::material::{check_admissible, encode_for_location, MaterialRefusal};
use crate::placeholder::{is_label, parse_exact, PLACEHOLDER_PREFIX};
use crate::target::{authority_matches_connect, ConnectAuthority, RuleTarget, TargetRefusal};
use zeroize::Zeroizing;

/// Where a key is substituted (P2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// The `Authorization` value is exactly `Bearer <placeholder>`, with the
    /// scheme matched case-insensitively, or a credential the caller parsed
    /// from such a value (see [`HeaderValueForm`]).
    Bearer,
    /// The whole value of the named header.
    Header(String),
    /// The whole value of the named query parameter, after percent-decoding.
    Query(String),
    /// An RFC 6901 pointer to a JSON string whose whole value is the
    /// placeholder.
    Json(String),
}

/// The vault's location spellings (wire-contract §15 `secret.deposit`):
/// `authorization_bearer`, `header:<name>`, `query:<name>` or
/// `json_pointer:<pointer>`.
///
/// AFT reads them from a `secret.use` release, which names where to put the
/// released key. The crate's own [`location_kind_token`] spelling stays as it
/// is: audit records name the location they evaluated, and that vocabulary is
/// already committed.
pub fn parse_vault_location(token: &str) -> Option<Location> {
    if token == "authorization_bearer" {
        return Some(Location::Bearer);
    }
    let (prefix, name) = token.split_once(':')?;
    if name.is_empty() {
        return None;
    }
    match prefix {
        "header" => Some(Location::Header(name.to_string())),
        "query" => Some(Location::Query(name.to_string())),
        "json_pointer" => Some(Location::Json(name.to_string())),
        _ => None,
    }
}

/// Short name of a location kind, used in audit records.
pub fn location_kind_token(location: &Location) -> String {
    match location {
        Location::Bearer => "bearer".to_string(),
        Location::Header(name) => format!("header:{}", name.to_ascii_lowercase()),
        Location::Query(name) => format!("query:{name}"),
        Location::Json(pointer) => format!("json:{pointer}"),
    }
}

/// A label's rule, as resolved by the vault (V1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRule {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub path_prefix: String,
    pub location: Location,
    /// Opaque grant identifier; the vault compares it, the crate never parses
    /// it.
    pub grant_id: String,
}

/// Why a request is refused (P2, P4, and the target rules it depends on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRefusal {
    /// A duplicate of a declared header or query parameter exists (P4).
    DuplicateField,
    /// A declared header name is not a valid HTTP field name (P2).
    InvalidHeaderName,
    /// A declared header name may not carry a key (P2).
    ReservedHeaderName,
    /// A JSON pointer is malformed (P2, P4).
    InvalidJsonPointer,
    /// A pointer names something other than a single string (P4).
    NonStringJsonTarget,
    /// The JSON body cannot be parsed (P4).
    MalformedJsonBody,
    /// The body is over the 1 MiB cap (P4).
    BodyTooLarge,
    /// The body nests deeper than the duplicate check will walk (P4).
    BodyTooDeep,
    /// The request body carries a Content-Encoding (P4).
    ContentEncodingPresent,
    /// A JSON body's Content-Type is not `application/json` or `+json` (P4).
    JsonContentTypeMissing,
    /// The request target and the rule disagree on host, scheme, port or path
    /// (M1, M2).
    TargetMismatch(TargetRefusal),
    /// The request's own authority is not the connection's CONNECT authority
    /// (M4).
    AuthorityMismatch,
    /// A key cannot be used at its location (P3).
    Material(MaterialRefusal),
    /// No key is held for a label the request names.
    MissingMaterial,
    /// A rule label does not satisfy the placeholder grammar (P1).
    InvalidLabel,
    /// The request target carries a fragment, which a server never sees.
    FragmentInTarget,
    /// The label's placeholder would disclose an installed key.
    ReplacementDisclosesKey,
    /// A whole `Authorization` value is this label's bare placeholder, with no
    /// `Bearer` scheme. A bearer rule substitutes only `Bearer <placeholder>`.
    BearerSchemeMissing,
}

impl From<MaterialRefusal> for RequestRefusal {
    fn from(value: MaterialRefusal) -> Self {
        RequestRefusal::Material(value)
    }
}

impl RequestRefusal {
    /// Stable reason tag for audit records.
    pub fn reason(&self) -> &'static str {
        match self {
            RequestRefusal::DuplicateField => "duplicate_field",
            RequestRefusal::InvalidHeaderName => "invalid_header_name",
            RequestRefusal::ReservedHeaderName => "reserved_header_name",
            RequestRefusal::InvalidJsonPointer => "invalid_json_pointer",
            RequestRefusal::NonStringJsonTarget => "non_string_json_target",
            RequestRefusal::MalformedJsonBody => "malformed_json_body",
            RequestRefusal::BodyTooLarge => "body_too_large",
            RequestRefusal::BodyTooDeep => "body_too_deep",
            RequestRefusal::ContentEncodingPresent => "content_encoding",
            RequestRefusal::JsonContentTypeMissing => "not_json",
            RequestRefusal::TargetMismatch(TargetRefusal::SchemeNotHttps) => "scheme_not_https",
            RequestRefusal::TargetMismatch(TargetRefusal::HostMismatch) => "host_mismatch",
            RequestRefusal::TargetMismatch(TargetRefusal::PortMismatch) => "port_mismatch",
            RequestRefusal::TargetMismatch(TargetRefusal::PathOutsidePrefix) => {
                "path_outside_prefix"
            }
            RequestRefusal::TargetMismatch(TargetRefusal::AmbiguousPathSeparator) => {
                "ambiguous_separator"
            }
            RequestRefusal::TargetMismatch(TargetRefusal::EmptySegment) => "empty_segment",
            RequestRefusal::TargetMismatch(TargetRefusal::NotOriginForm) => "not_origin_form",
            RequestRefusal::TargetMismatch(TargetRefusal::MalformedPath) => "malformed_path",
            RequestRefusal::TargetMismatch(TargetRefusal::MalformedAuthority) => {
                "malformed_authority"
            }
            RequestRefusal::TargetMismatch(TargetRefusal::FragmentInTarget) => "fragment_in_target",
            RequestRefusal::AuthorityMismatch => "authority_mismatch",
            RequestRefusal::Material(MaterialRefusal::Empty) => "material_empty",
            RequestRefusal::Material(MaterialRefusal::TooShort) => "material_too_short",
            RequestRefusal::Material(MaterialRefusal::SubstringOfPlaceholder) => {
                "material_in_placeholder"
            }
            RequestRefusal::TargetMismatch(TargetRefusal::AuthorityMismatch) => {
                "authority_mismatch"
            }
            RequestRefusal::Material(MaterialRefusal::NonVisibleHeaderByte) => {
                "material_not_visible"
            }
            RequestRefusal::MissingMaterial => "missing_material",
            RequestRefusal::InvalidLabel => "invalid_label",
            RequestRefusal::FragmentInTarget => "fragment_in_target",
            RequestRefusal::ReplacementDisclosesKey => "replacement_discloses_key",
            RequestRefusal::BearerSchemeMissing => "bearer_scheme_missing",
        }
    }
}

/// How one label was decided for one request.
///
/// The variant is reported in full, but `Debug` never prints replacement bytes:
/// an outcome can be logged while the request is still being assembled.
#[derive(Clone, PartialEq, Eq)]
pub enum LabelOutcome {
    /// The rule matched and the placeholder was substituted.
    Substituted {
        byte_start: usize,
        byte_end: usize,
        replacement: Zeroizing<Vec<u8>>,
    },
    /// The rule matched but the placeholder is not the whole value (P2): it
    /// goes upstream literally.
    NotWholeValue,
    /// The rule did not match; the placeholder goes upstream literally.
    OutOfScope,
    /// The label was refused; the request is not sent.
    Refused(RequestRefusal),
}

impl std::fmt::Debug for LabelOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LabelOutcome::Substituted {
                byte_start,
                byte_end,
                replacement,
            } => f
                .debug_struct("Substituted")
                .field("byte_start", byte_start)
                .field("byte_end", byte_end)
                .field("replacement_len", &replacement.len())
                .finish(),
            LabelOutcome::NotWholeValue => f.write_str("NotWholeValue"),
            LabelOutcome::OutOfScope => f.write_str("OutOfScope"),
            LabelOutcome::Refused(refusal) => f.debug_tuple("Refused").field(refusal).finish(),
        }
    }
}

impl LabelOutcome {
    /// True when this label was substituted.
    pub fn is_substituted(&self) -> bool {
        matches!(self, LabelOutcome::Substituted { .. })
    }
}

/// One byte edit: replace `[start, end)` with `replacement`.
#[derive(Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edit")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("replacement_len", &self.replacement.len())
            .finish()
    }
}

/// What a [`HeaderEdit`]'s replacement stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderEditKind {
    /// The replacement is the complete new field value. For a
    /// [`Location::Bearer`] rule it keeps the `Bearer ` scheme exactly as the
    /// request spelled it, followed by the key.
    WholeValue,
    /// The replacement is the bearer credential only, because the caller
    /// passed the credential through [`HeaderField::bearer_credential`] or
    /// [`HeaderField::ranged_bearer_credential`]; the caller keeps the scheme
    /// it parsed.
    BearerCredential,
}

/// One header value to replace, and what it becomes.
///
/// For a ranged field, `[start, end)` is the range the caller supplied in
/// `header_bytes`, and `replacement` replaces exactly those bytes. For a
/// borrowed field there are no offsets: `start` and `end` are both zero and the
/// caller rewrites the field by name, using `replacement` as described by
/// `kind`.
///
/// `Debug` prints the name's length rather than the name: a header rule's name
/// is caller-chosen text and is never checked against the keys.
#[derive(Clone, PartialEq, Eq)]
pub struct HeaderEdit {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub replacement: Zeroizing<Vec<u8>>,
    pub kind: HeaderEditKind,
}

impl std::fmt::Debug for HeaderEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeaderEdit")
            .field("name_len", &self.name.len())
            .field("start", &self.start)
            .field("end", &self.end)
            .field("replacement_len", &self.replacement.len())
            .field("kind", &self.kind)
            .finish()
    }
}

/// The result of evaluating every rule against one request.
///
/// `Debug` reports sizes and decisions only: `rewritten_body` holds the key for
/// a JSON substitution, and the label and header edits hold it too.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestOutcome {
    /// One entry per rule, in the caller's rule order.
    pub labels: Vec<LabelOutcome>,
    /// Edits to the query inside the request target, sorted by offset.
    pub target_edits: Vec<Edit>,
    /// Edits to header values inside the request buffer, in rule order.
    pub header_edits: Vec<HeaderEdit>,
    /// The body to send when a JSON pointer changed it.
    pub rewritten_body: Option<Zeroizing<Vec<u8>>>,
    /// Set when any label refused: nothing is sent upstream, every
    /// key-bearing edit is dropped, and any material fetched for the request is
    /// zeroized.
    pub refusal: Option<RequestRefusal>,
}

impl std::fmt::Debug for RequestOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestOutcome")
            .field("labels", &self.labels)
            .field("target_edits", &self.target_edits)
            .field("header_edits", &self.header_edits)
            .field(
                "rewritten_body_len",
                &self.rewritten_body.as_ref().map(|body| body.len()),
            )
            .field("refusal", &self.refusal)
            .finish()
    }
}

impl RequestOutcome {
    /// True when at least one location was substituted.
    ///
    /// Derived from the label outcomes, so a header or bearer substitution
    /// counts exactly like a query or body one, and a refused request never
    /// reports a substitution.
    pub fn substituted(&self) -> bool {
        self.refusal.is_none() && self.labels.iter().any(LabelOutcome::is_substituted)
    }
}

/// What the bytes of a [`HeaderField`] are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderValueForm {
    /// The whole field value as it appears on the wire. An `Authorization`
    /// value in this form must carry its `Bearer ` scheme to be substituted;
    /// a bare placeholder with no scheme is refused.
    WholeValue,
    /// Only the credential of an `Authorization` value whose `Bearer` scheme
    /// the caller has already parsed and will keep.
    BearerCredential,
}

/// A header field of a request under evaluation.
#[derive(Debug, Clone)]
pub struct HeaderField<'a> {
    pub name: &'a str,
    /// Byte range of the value inside the request buffer, when the caller has
    /// one contiguous buffer.
    pub value_range: Option<(usize, usize)>,
    /// The value bytes, for callers that evaluate a decision and rewrite it
    /// elsewhere (fetch).
    pub borrowed: Option<&'a [u8]>,
    /// Whether the bytes are the whole value or a parsed bearer credential.
    pub form: HeaderValueForm,
}

impl<'a> HeaderField<'a> {
    /// A header whose whole value bytes are known but whose position is not.
    pub fn value(name: &'a str, value: &'a [u8]) -> Self {
        Self {
            name,
            value_range: None,
            borrowed: Some(value),
            form: HeaderValueForm::WholeValue,
        }
    }

    /// A header whose whole value is a range inside the request buffer.
    pub fn ranged(name: &'a str, start: usize, end: usize) -> Self {
        Self {
            name,
            value_range: Some((start, end)),
            borrowed: None,
            form: HeaderValueForm::WholeValue,
        }
    }

    /// The credential of an `Authorization: Bearer` value, already split from
    /// its scheme by the caller.
    pub fn bearer_credential(credential: &'a [u8]) -> Self {
        Self {
            name: "Authorization",
            value_range: None,
            borrowed: Some(credential),
            form: HeaderValueForm::BearerCredential,
        }
    }

    /// The credential of an `Authorization: Bearer` value as a range inside
    /// the request buffer; the range covers the credential only.
    pub fn ranged_bearer_credential(start: usize, end: usize) -> Self {
        Self {
            name: "Authorization",
            value_range: Some((start, end)),
            borrowed: None,
            form: HeaderValueForm::BearerCredential,
        }
    }
}

/// A request to evaluate.
///
/// Header values and the body are separate inputs on purpose: a ranged header
/// value is read from `header_bytes` (the header block), never from the body.
/// Handing the body to a header read would let a request body decide the
/// `Host` the CONNECT check sees, or a header decide what a JSON location reads.
#[derive(Debug, Clone)]
pub struct RequestView<'a> {
    /// The request target, exactly as received.
    pub target: &'a str,
    pub headers: Vec<HeaderField<'a>>,
    /// The buffer ranged header values index into.
    pub header_bytes: &'a [u8],
    /// The request body, and only the body.
    pub body: &'a [u8],
}

impl<'a> RequestView<'a> {
    /// A request whose header values are carried borrowed, not ranged.
    pub fn new(target: &'a str, headers: Vec<HeaderField<'a>>) -> Self {
        Self {
            target,
            headers,
            header_bytes: &[],
            body: &[],
        }
    }

    /// A request whose header values are ranges inside `header_bytes`.
    pub fn from_header_block(
        target: &'a str,
        headers: Vec<HeaderField<'a>>,
        header_bytes: &'a [u8],
    ) -> Self {
        Self {
            target,
            headers,
            header_bytes,
            body: &[],
        }
    }

    /// Attach a request body (never used to read header values).
    pub fn with_body(mut self, body: &'a [u8]) -> Self {
        self.body = body;
        self
    }

    /// The header block this request was built with.
    pub fn header_bytes(&self) -> &'a [u8] {
        self.header_bytes
    }

    /// The values of every header called `name`, in wire order.
    ///
    /// A ranged value is read from the request buffer the caller supplied; a
    /// borrowed value is used as-is. Keeping those two paths apart matters:
    /// reading a ranged field from the wrong slice silently replaces the value
    /// with an unrelated one.
    fn header_values<'b>(&'b self, name: &'b str) -> Vec<&'b [u8]> {
        self.headers
            .iter()
            .filter(|field| field.name.eq_ignore_ascii_case(name))
            .map(|field| self.field_value(field))
            .collect()
    }

    /// The bytes of one field: its range in `header_bytes`, or its borrowed
    /// value. Never the body.
    fn field_value<'b>(&'b self, field: &'b HeaderField<'_>) -> &'b [u8] {
        match field.value_range {
            Some((start, end)) => self.header_bytes.get(start..end).unwrap_or_default(),
            None => field.borrowed.unwrap_or_default(),
        }
    }
}

/// Largest request body a JSON location may be declared against (P4).
pub const MAX_SUBSTITUTABLE_BODY: usize = 1024 * 1024;

/// Header names that may never carry a key (P2).
pub const RESERVED_HEADER_NAMES: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "upgrade",
    "te",
    "trailer",
    "accept-encoding",
    "cookie",
];

/// True when `name` is reserved, a `Sec-WebSocket-*` name, or a pseudo-header.
pub fn header_name_is_reserved(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.starts_with(':')
        || lowered.starts_with("sec-websocket-")
        || RESERVED_HEADER_NAMES.contains(&lowered.as_str())
}

/// Validate a rule's header name for P2.
pub fn validate_header_name(name: &str) -> Result<(), RequestRefusal> {
    let bytes = name.as_bytes();
    // A reserved name is reported as reserved even when it is also a
    // pseudo-header (`:authority`), because the rule that matters is P2's.
    if header_name_is_reserved(name) {
        return Err(RequestRefusal::ReservedHeaderName);
    }
    if bytes.is_empty() || !bytes.iter().all(|byte| is_token_byte(*byte)) {
        return Err(RequestRefusal::InvalidHeaderName);
    }
    Ok(())
}

/// RFC 7230 `tchar`.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// True when the placeholder this rule writes would disclose an installed key.
///
/// The replacement is generated text, so it is never rescanned; but the label
/// itself is caller-supplied, and a label that spells a key (its hex, its
/// base64, or the key itself) would write that key into every response it
/// appears in. The check covers every supplied key, the rule's own included,
/// in every spelling the scrubber recognises; such a pair refuses instead of
/// being written. A key that contains the whole placeholder refuses too, as
/// P3 does for a rule's own key.
fn replacement_collides(rule: &LabelRule, keys: &[(String, Vec<u8>)]) -> Option<RequestRefusal> {
    let placeholder = crate::placeholder::placeholder_text(&rule.label);
    keys.iter()
        .any(|(_, key)| {
            !key.is_empty()
                && (crate::scrub::replacement_discloses(placeholder.as_bytes(), key)
                    || crate::material::key_is_substring_of_placeholder(key, &rule.label))
        })
        .then_some(RequestRefusal::ReplacementDisclosesKey)
}

/// What `evaluate_location` decided for one non-JSON rule.
enum Decision {
    /// A substitute decision with the value span inside the request buffer,
    /// what the replacement stands for, and the location token the audit
    /// record needs.
    Substituted {
        start: usize,
        end: usize,
        replacement: Vec<u8>,
        kind: HeaderEditKind,
        location: String,
    },
    NotWhole,
    OutOfScope,
}

/// Decide one non-JSON location. The key is already known to belong to the
/// rule's label.
fn decide_location<'q>(
    rule: &'q LabelRule,
    request: &RequestView<'_>,
    query: &mut QueryIndex<'q>,
    key: &[u8],
) -> Result<Decision, RequestRefusal> {
    let placeholder = crate::placeholder::placeholder_text(&rule.label);
    match &rule.location {
        Location::Bearer => {
            let fields: Vec<&HeaderField<'_>> = request
                .headers
                .iter()
                .filter(|field| field.name.eq_ignore_ascii_case("authorization"))
                .collect();
            if fields.len() > 1 {
                return Err(RequestRefusal::DuplicateField);
            }
            let Some(field) = fields.first() else {
                return Ok(Decision::OutOfScope);
            };
            let value = request.field_value(field);
            // A borrowed value has no offsets: the edit covers nothing and the
            // caller rewrites the field by name.
            let (start, end) = field.value_range.unwrap_or((0, 0));
            match bearer_form(value, &rule.label, field.form) {
                BearerForm::Whole { scheme_len } => {
                    check_admissible(key, &rule.label, &rule.location)?;
                    // A whole wire value is rewritten whole, keeping the
                    // scheme as the request spelled it; a parsed credential is
                    // replaced by the key alone and the caller keeps its scheme.
                    let mut replacement = value[..scheme_len].to_vec();
                    replacement.extend_from_slice(key);
                    let kind = match field.form {
                        HeaderValueForm::WholeValue => HeaderEditKind::WholeValue,
                        HeaderValueForm::BearerCredential => HeaderEditKind::BearerCredential,
                    };
                    Ok(Decision::Substituted {
                        start,
                        end,
                        replacement,
                        kind,
                        location: "bearer".to_string(),
                    })
                }
                BearerForm::SchemeMissing => Err(RequestRefusal::BearerSchemeMissing),
                BearerForm::NotWhole => Ok(Decision::NotWhole),
                BearerForm::Absent => Ok(Decision::OutOfScope),
            }
        }
        Location::Header(name) => {
            validate_header_name(name)?;
            let values = request.header_values(name);
            if values.len() > 1 {
                return Err(RequestRefusal::DuplicateField);
            }
            let Some(value) = values.first() else {
                return Ok(Decision::OutOfScope);
            };
            if *value == placeholder.as_bytes() {
                check_admissible(key, &rule.label, &rule.location)?;
                // A borrowed value has no offsets: the edit covers nothing and
                // the caller rewrites the header by name.
                let (start, end) = value_range(request, name)?.unwrap_or((0, 0));
                Ok(Decision::Substituted {
                    start,
                    end,
                    replacement: key.to_vec(),
                    kind: HeaderEditKind::WholeValue,
                    location: location_kind_token(&rule.location),
                })
            } else if mentions_placeholder(value) {
                Ok(Decision::NotWhole)
            } else {
                Ok(Decision::OutOfScope)
            }
        }
        Location::Query(name) => {
            if query.is_claimed(name) {
                return Err(RequestRefusal::DuplicateField);
            }
            query.claim(name);
            let mut fields = query.fields_named(name);
            let first = fields.next();
            if fields.next().is_some() {
                return Err(RequestRefusal::DuplicateField);
            }
            let Some((start, end, decoded)) = first else {
                return Ok(Decision::OutOfScope);
            };
            if decoded == placeholder.as_bytes() {
                let replacement = encode_for_location(key, &rule.label, &rule.location)?;
                Ok(Decision::Substituted {
                    start,
                    end,
                    replacement,
                    kind: HeaderEditKind::WholeValue,
                    location: location_kind_token(&rule.location),
                })
            } else if mentions_placeholder(&decoded) {
                Ok(Decision::NotWhole)
            } else {
                Ok(Decision::OutOfScope)
            }
        }
        Location::Json(_) => Ok(Decision::OutOfScope),
    }
}

/// Evaluate every rule against one request.
///
/// `keys` holds one key per label; a rule whose label has no key refuses the
/// request rather than reusing another label's key. The connection's CONNECT
/// authority is a required input: a request whose own authority disagrees with
/// it is refused before any location is considered (M4).
pub fn evaluate_request_with_connect(
    request: &RequestView<'_>,
    connect: &ConnectAuthority,
    rules: &[LabelRule],
    keys: &[(String, Vec<u8>)],
) -> RequestOutcome {
    let mut refusal = check_authority(request, connect);
    if refusal.is_none() && request.target.contains('#') {
        refusal = Some(RequestRefusal::FragmentInTarget);
    }
    // The destination comes from the CONNECT authority and the request target,
    // never from the caller: a rule is matched against where the connection
    // actually goes.
    let bound = bound_target(request, connect);
    if refusal.is_none() {
        if let Err(err) = &bound {
            refusal = Some(*err);
        }
    }
    let mut outcomes: Vec<LabelOutcome> = Vec::with_capacity(rules.len());

    let mut query = QueryIndex::parse(request.target);
    // JSON locations are evaluated as one pass over the body, so their slots
    // are recorded as (rule index, outcome) and merged after that pass.
    let mut json_rules: Vec<(usize, &LabelRule)> = Vec::new();
    let mut header_edits: Vec<HeaderEdit> = Vec::new();

    for rule in rules {
        if refusal.is_none() {
            refusal = validate_rule(rule);
        }
        if let Some(err) = refusal {
            outcomes.push(LabelOutcome::Refused(err));
            continue;
        }
        if let Some(collision) = replacement_collides(rule, keys) {
            refusal = Some(collision);
            outcomes.push(LabelOutcome::Refused(collision));
            continue;
        }
        let rule_target = RuleTarget {
            host: &rule.host,
            port: rule.port,
            path_prefix: &rule.path_prefix,
        };
        let matched = bound
            .as_ref()
            .ok()
            .is_some_and(|target| target.matches(&rule_target).is_ok());
        if !matched {
            // A target miss is the label's own business; it never refuses the
            // request, because the placeholder then goes upstream literally.
            outcomes.push(LabelOutcome::OutOfScope);
            continue;
        }
        let Some(key) = key_for(keys, &rule.label) else {
            refusal = Some(RequestRefusal::MissingMaterial);
            outcomes.push(LabelOutcome::Refused(RequestRefusal::MissingMaterial));
            continue;
        };
        if let Location::Json(_) = rule.location {
            json_rules.push((outcomes.len(), rule));
            outcomes.push(LabelOutcome::OutOfScope);
            continue;
        }
        match decide_location(rule, request, &mut query, key) {
            Ok(Decision::Substituted {
                start,
                end,
                replacement,
                kind,
                location,
            }) => {
                // A query or body edit is reported in its own place; only a
                // header value becomes a header edit.
                let header_name = match &rule.location {
                    Location::Bearer => Some("Authorization".to_string()),
                    Location::Header(name) => Some(name.to_ascii_lowercase()),
                    Location::Query(_) | Location::Json(_) => None,
                };
                if let Some(name) = header_name {
                    header_edits.push(HeaderEdit {
                        name,
                        start,
                        end,
                        replacement: Zeroizing::new(replacement.clone()),
                        kind,
                    });
                }
                let _ = location;
                outcomes.push(LabelOutcome::Substituted {
                    byte_start: start,
                    byte_end: end,
                    replacement: Zeroizing::new(replacement),
                });
            }
            Ok(Decision::NotWhole) => outcomes.push(LabelOutcome::NotWholeValue),
            Ok(Decision::OutOfScope) => outcomes.push(LabelOutcome::OutOfScope),
            Err(err) => {
                refusal = refusal.or(Some(err));
                outcomes.push(LabelOutcome::Refused(err));
            }
        }
    }

    let mut rewritten_body = None;
    if !json_rules.is_empty() && refusal.is_none() {
        let rules_only: Vec<&LabelRule> = json_rules.iter().map(|(_, rule)| *rule).collect();
        let keys_only: Vec<(String, Vec<u8>)> = rules_only
            .iter()
            .filter_map(|rule| {
                key_for(keys, &rule.label).map(|key| (rule.label.clone(), key.to_vec()))
            })
            .collect();
        match json::evaluate(request, &rules_only, &keys_only) {
            Ok(result) => {
                for ((slot, _), outcome) in json_rules.iter().zip(result.outcomes) {
                    outcomes[*slot] = outcome;
                }
                rewritten_body = result.rewritten_body;
            }
            Err(err) => {
                refusal = refusal.or(Some(err));
                outcomes = rules.iter().map(|_| LabelOutcome::Refused(err)).collect();
                rewritten_body = None;
            }
        }
    } else if let Some(err) = refusal {
        for (slot, _) in &json_rules {
            outcomes[*slot] = LabelOutcome::Refused(err);
        }
    }

    // Query edits carry offsets into the request target; header edits carry
    // offsets into the header buffer and are reported separately.
    let mut target_edits: Vec<Edit> = outcomes
        .iter()
        .zip(rules.iter())
        .filter_map(|(outcome, rule)| match (outcome, &rule.location) {
            (
                LabelOutcome::Substituted {
                    byte_start,
                    byte_end,
                    replacement,
                },
                Location::Query(_),
            ) => Some(Edit {
                start: *byte_start,
                end: *byte_end,
                replacement: replacement.clone(),
            }),
            _ => None,
        })
        .collect();
    target_edits.sort_by_key(|edit| edit.start);

    finish(
        outcomes,
        target_edits,
        header_edits,
        rewritten_body,
        refusal,
    )
}

/// A refused request carries no edits at all: every key-bearing edit is dropped
/// so a caller that applies edits without checking the refusal cannot send a
/// key.
fn finish(
    labels: Vec<LabelOutcome>,
    target_edits: Vec<Edit>,
    header_edits: Vec<HeaderEdit>,
    rewritten_body: Option<Zeroizing<Vec<u8>>>,
    refusal: Option<RequestRefusal>,
) -> RequestOutcome {
    if let Some(err) = refusal {
        let labels = labels
            .into_iter()
            .map(|outcome| match outcome {
                LabelOutcome::Substituted { .. } => LabelOutcome::Refused(err),
                other => other,
            })
            .collect();
        return RequestOutcome {
            labels,
            target_edits: Vec::new(),
            header_edits: Vec::new(),
            rewritten_body: None,
            refusal: Some(err),
        };
    }
    RequestOutcome {
        labels,
        target_edits,
        header_edits,
        rewritten_body,
        refusal,
    }
}

fn key_for<'k>(keys: &'k [(String, Vec<u8>)], label: &str) -> Option<&'k [u8]> {
    keys.iter()
        .find(|(key_label, _)| key_label == label)
        .map(|(_, key)| key.as_slice())
}

/// M4: the request's own authority must match the connection's.
///
/// Every `Host` and `:authority` field is checked, not just the first: a
/// request carrying two of them leaves it to the server which one to trust, and
/// AFT must not authorize one value while forwarding another.
fn check_authority(
    request: &RequestView<'_>,
    connect: &ConnectAuthority,
) -> Option<RequestRefusal> {
    let header = |name: &str| -> Vec<String> {
        request
            .header_values(name)
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect()
    };
    let hosts = header("host");
    let authorities = header(":authority");
    if hosts.len() > 1 || authorities.len() > 1 {
        return Some(RequestRefusal::DuplicateField);
    }
    // A range that does not fit the header block means the caller's header and
    // body inputs disagree; refuse rather than compare a truncated value.
    let ranged_outside = request.headers.iter().any(|field| match field.value_range {
        Some((start, end)) => end > request.header_bytes.len() || start > end,
        None => false,
    });
    if ranged_outside {
        return Some(RequestRefusal::TargetMismatch(
            crate::target::TargetRefusal::AuthorityMismatch,
        ));
    }
    authority_matches_connect(
        connect,
        hosts.first().map(String::as_str),
        authorities.first().map(String::as_str),
        request.target,
    )
    .err()
    .map(RequestRefusal::TargetMismatch)
}

/// The target a rule is matched against, derived from the connection and the
/// request rather than trusted from the caller.
///
/// The host and port come from the CONNECT authority, because that is where the
/// connection actually goes; the path comes from the request target, and a
/// fragment in it is refused (a server never receives one, and a target that
/// carries one could match a prefix while being forwarded byte for byte).
fn bound_target<'a>(
    request: &RequestView<'a>,
    connect: &ConnectAuthority,
) -> Result<crate::target::RequestTarget<'a>, RequestRefusal> {
    if request.target.contains('#') {
        return Err(RequestRefusal::FragmentInTarget);
    }
    Ok(crate::target::RequestTarget {
        host: connect.host.clone(),
        port: connect.port,
        scheme: "https",
        target: request.target,
    })
}

/// Rules whose fields the vault must never be able to smuggle past the grammar.
pub(crate) fn validate_rule(rule: &LabelRule) -> Option<RequestRefusal> {
    if !is_label(&rule.label) {
        return Some(RequestRefusal::InvalidLabel);
    }
    if let Location::Header(name) = &rule.location {
        if let Err(err) = validate_header_name(name) {
            return Some(err);
        }
    }
    // The prefix must be exactly the normalized path the rule declares. A
    // doubled separator, a dot segment or a trailing slash is a second spelling
    // for a location, so it refuses instead of being normalised for the rule.
    if crate::target::normalize_path_prefix(&rule.path_prefix)
        .map_or(true, |normalized| normalized != rule.path_prefix)
    {
        return Some(RequestRefusal::TargetMismatch(TargetRefusal::MalformedPath));
    }
    None
}

/// The byte range of a header value in the header block, when the caller
/// supplied one. A caller that only asks for a decision (fetch) passes
/// borrowed values and no ranges; it rewrites its own header block by name.
fn value_range(
    request: &RequestView<'_>,
    name: &str,
) -> Result<Option<(usize, usize)>, RequestRefusal> {
    Ok(request
        .headers
        .iter()
        .find(|field| field.name.eq_ignore_ascii_case(name))
        .and_then(|field| field.value_range))
}

#[derive(Debug)]
enum BearerForm {
    /// The credential is exactly this label's placeholder. `scheme_len` bytes
    /// of the value (`Bearer ` for a whole value, none for a parsed
    /// credential) come before it and are kept in the replacement.
    Whole {
        scheme_len: usize,
    },
    /// A whole wire value that is exactly this label's placeholder, with no
    /// `Bearer` scheme at all.
    SchemeMissing,
    NotWhole,
    Absent,
}

/// Decide how an `Authorization` value relates to `label`.
///
/// `form` says whether `value` is the complete field value or a credential
/// whose `Bearer` scheme the caller already parsed. A complete value must start
/// with `Bearer ` (scheme case-insensitive); one that is the bare placeholder
/// is not treated as a bearer credential, because the scheme it would be sent
/// with is not known, and is refused instead.
fn bearer_form(value: &[u8], label: &str, form: HeaderValueForm) -> BearerForm {
    let is_this_placeholder = |text: &[u8]| {
        std::str::from_utf8(text)
            .ok()
            .and_then(parse_exact)
            .is_some_and(|parsed| parsed.label == label)
    };
    let (scheme_len, credential) = match form {
        HeaderValueForm::BearerCredential => (0, value),
        HeaderValueForm::WholeValue => {
            let has_scheme = value.get(..7).is_some_and(|scheme| {
                scheme[..6].eq_ignore_ascii_case(b"Bearer") && scheme[6] == b' '
            });
            if !has_scheme {
                return if is_this_placeholder(value) {
                    BearerForm::SchemeMissing
                } else if mentions_placeholder(value) {
                    BearerForm::NotWhole
                } else {
                    BearerForm::Absent
                };
            }
            (7, &value[7..])
        }
    };
    // The credential must be exactly this label's placeholder. Anything else
    // that looks like a placeholder goes upstream literally rather than being
    // guessed at.
    let parsed_mark = std::str::from_utf8(credential).ok().and_then(parse_exact);
    match parsed_mark {
        Some(parsed) if parsed.label == label => BearerForm::Whole { scheme_len },
        Some(_) => BearerForm::NotWhole,
        None if mentions_placeholder(credential) => BearerForm::NotWhole,
        None => BearerForm::Absent,
    }
}

fn mentions_placeholder(value: &[u8]) -> bool {
    value
        .windows(PLACEHOLDER_PREFIX.len())
        .any(|window| window == PLACEHOLDER_PREFIX.as_bytes())
}

/// Query parameters with their byte ranges in the request target.
///
/// Names are compared **decoded**, because a server decodes them: `k%65y` and
/// `key` are the same parameter to every form and query parser, and treating
/// them as different would let a second value shadow the authorised one.
struct QueryIndex<'a> {
    text: &'a str,
    /// `(decoded name, value start, value end)` in target order.
    fields: Vec<(String, usize, usize)>,
    claimed: Vec<String>,
}

impl<'a> QueryIndex<'a> {
    fn parse(target: &'a str) -> Self {
        let mut fields = Vec::new();
        // A fragment is never sent to a server; when a target carries one, it
        // is not part of the query at all.
        let without_fragment = match target.find('#') {
            Some(at) => &target[..at],
            None => target,
        };
        if let Some(query_start) = without_fragment.find('?') {
            let query = &without_fragment[query_start + 1..];
            let mut offset = query_start + 1;
            for pair in query.split('&') {
                let pair_start = offset;
                offset += pair.len() + 1;
                if pair.is_empty() {
                    continue;
                }
                let (raw_name, value_start, value_end) = match pair.find('=') {
                    Some(eq) => (&pair[..eq], pair_start + eq + 1, pair_start + pair.len()),
                    None => (pair, pair_start + pair.len(), pair_start + pair.len()),
                };
                let name = String::from_utf8_lossy(&crate::placeholder::percent_decode_lossy(
                    raw_name.as_bytes(),
                ))
                .into_owned();
                fields.push((name, value_start, value_end));
            }
        }
        Self {
            text: target,
            fields,
            claimed: Vec::new(),
        }
    }

    fn is_claimed(&self, name: &str) -> bool {
        self.claimed.iter().any(|claimed| claimed == name)
    }

    fn claim(&mut self, name: &str) {
        if !self.is_claimed(name) {
            self.claimed.push(name.to_string());
        }
    }

    fn fields_named<'b>(
        &'b self,
        name: &'b str,
    ) -> impl Iterator<Item = (usize, usize, Vec<u8>)> + 'b {
        self.fields
            .iter()
            .filter(move |(field, _, _)| field == name)
            .map(|(_, start, end)| {
                let raw = &self.text.as_bytes()[*start..*end];
                (*start, *end, crate::placeholder::percent_decode_lossy(raw))
            })
    }
}

/// Apply target-query edits to a request target.
pub fn apply_target_edits(target: &str, edits: &[Edit]) -> String {
    let bytes = target.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    for edit in edits {
        out.extend_from_slice(&bytes[cursor..edit.start]);
        out.extend_from_slice(&edit.replacement);
        cursor = edit.end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    String::from_utf8_lossy(&out).into_owned()
}
