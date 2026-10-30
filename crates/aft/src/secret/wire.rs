//! The `secret.use` wire contract (wire-contract v1, §15 "Agent secrets").
//!
//! AFT is a **sender**: it calls `secret.use` for the agent scope it is serving
//! and receives material only for a call a live grant covers. The request is
//! `{name, coverage?, stamp, target}`, and the stamp is the daemon's own
//! `ScopeStamp`, relayed exactly as subc delivered it. There is no agent-name
//! parameter: AFT holds no trusted human-readable name for an agent, so the
//! vault names the agent only by the stamp's `attributes.agent_id`.
//!
//! Every key set here is **closed**. A request missing a required key, or
//! carrying any key not shown, is refused with `invalid_params`. These types
//! exist so AFT's outbound request cannot carry an extra key by accident, and
//! so a reply is parsed strictly: an unknown or missing reply key is a protocol
//! error, not something to guess about.
//!
//! The three reply forms are pinned by the contract's golden examples, which
//! `crates/aft/tests/fixtures/secret/secret-approval-v1.json` carries and the
//! seam's tests check.

use std::fmt;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::scope::ScopeStamp;

/// Coverage default: one approval covers the asking scope.
pub const COVERAGE_SCOPE: &str = "scope";
/// Coverage for a Head and its workers; offered only `permanent`.
pub const COVERAGE_AGENT_AND_WORKERS: &str = "agent_and_workers";

/// Which approval coverage a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// One secret use for the asking scope (the default when omitted).
    Scope,
    /// The asking head and every worker under it.
    AgentAndWorkers,
}

impl Coverage {
    /// The wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Coverage::Scope => COVERAGE_SCOPE,
            Coverage::AgentAndWorkers => COVERAGE_AGENT_AND_WORKERS,
        }
    }
}

/// The target an API key is asked for: `{scheme, host, port?, path}`.
///
/// Senders strip the query and fragment before calling, so a `?` or `#` in
/// `path` is a grammar fault on the vault's side; AFT refuses to send one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyTarget {
    /// `https`, the only scheme the vault accepts.
    pub scheme: String,
    /// Lowercase ASCII host.
    pub host: String,
    /// The port; omitted on the wire when it is the default 443.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The request path, starting with `/` and carrying no query or fragment.
    pub path: String,
}

/// The target a login was asked for: `{origin}`, canonical `https://host`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoginTarget {
    /// The canonical origin, with port 443 omitted.
    pub origin: String,
}

/// The `target` member of a `secret.use` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An API key for one host, port and path.
    ApiKey(ApiKeyTarget),
    /// A site login, addressed by origin only.
    Login(LoginTarget),
}

impl Serialize for Target {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Target::ApiKey(target) => target.serialize(serializer),
            Target::Login(target) => target.serialize(serializer),
        }
    }
}

/// One `secret.use` request: `{name, coverage?, stamp, target}`.
///
/// The `stamp` is the daemon's `ScopeStamp` value, relayed byte for byte. AFT
/// never rewrites it, because the vault trusts the sender to pass it through
/// unchanged and takes the asking scope from nowhere else.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SecretUseRequest {
    /// The secret name (`[A-Za-z0-9][A-Za-z0-9._-]*`, 1–64 bytes).
    pub name: String,
    /// The coverage; `scope` when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// The asking call's daemon stamp, unchanged.
    pub stamp: ScopeStamp,
    /// What the key is being used for.
    pub target: Target,
}

impl SecretUseRequest {
    /// Build a request for the scope `stamp` names.
    ///
    /// The stamp is taken by value from the route identity, so a caller cannot
    /// substitute a stamp it typed.
    pub fn new(name: &str, stamp: &ScopeStamp, target: Target) -> Self {
        Self {
            name: name.to_string(),
            coverage: None,
            stamp: stamp.clone(),
            target,
        }
    }

    /// Ask for coverage other than the default.
    pub fn with_coverage(mut self, coverage: Coverage) -> Self {
        self.coverage = Some(coverage);
        self
    }

    /// The request as the `params` object of a `secret.use` call.
    pub fn to_params(&self) -> Value {
        serde_json::to_value(self).expect("a secret.use request serializes")
    }
}

/// Why a reply could not be read as one of the three documented forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyError {
    /// The reply is not one of the documented shapes, or carries a key the
    /// contract does not show.
    Malformed {
        /// Plain text naming the shape fault; never a value from the reply.
        detail: String,
    },
    /// `key_b64url` was not strict unpadded base64url.
    BadKeyEncoding,
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplyError::Malformed { detail } => write!(f, "malformed reply: {detail}"),
            ReplyError::BadKeyEncoding => {
                f.write_str("key_b64url is not strict unpadded base64url")
            }
        }
    }
}

/// An API key the vault released to AFT for this call.
pub struct ReleasedApiKey {
    /// Where to put the key: `authorization_bearer`, `header:<name>`,
    /// `query:<name>` or `json_pointer:<pointer>`.
    pub location: String,
    /// The key bytes, zeroizing.
    bytes: crate::secret::vault::Material,
}

impl ReleasedApiKey {
    /// The key bytes for the one request that may carry them.
    pub fn material(&self) -> &[u8] {
        self.bytes.expose()
    }
}

impl fmt::Debug for ReleasedApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReleasedApiKey")
            .field("location", &self.location)
            .field("key_len", &self.bytes.len())
            .finish()
    }
}

/// A site login the vault released; only Cerebellum asks for one.
pub struct ReleasedLogin {
    /// The account name.
    pub username: String,
    /// The password.
    password: crate::secret::vault::Material,
    /// The current TOTP code, when the record holds a seed. The seed itself
    /// never leaves the vault.
    pub totp: Option<String>,
}

impl ReleasedLogin {
    /// The password bytes.
    pub fn password(&self) -> &[u8] {
        self.password.expose()
    }
}

impl fmt::Debug for ReleasedLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReleasedLogin")
            .field("username_len", &self.username.len())
            .field("password_len", &self.password.len())
            .field("totp", &self.totp.is_some())
            .finish()
    }
}

/// What `secret.use` answered, as one of the contract's three forms.
#[derive(Debug)]
pub enum SecretUseReply {
    /// A live grant covered the call: substitute the key at `location`.
    ReleasedApiKey(ReleasedApiKey),
    /// A live grant released a login to Cerebellum.
    ReleasedLogin(ReleasedLogin),
    /// No live grant covers the call. The sender shows nothing from this to
    /// the model: the statement goes to the approval card, and the call is
    /// retried after the phone answers. `approval_required` does not mean a
    /// phone can currently approve.
    ApprovalRequired(ApprovalRequest),
}

/// The vault's pending approval (§15 "Signed statement and envelope").
#[derive(Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The vault's identifier for the pending request, a 26-character ULID.
    pub request_id: String,
    /// The signed approval statement, unpadded base64url.
    pub statement_b64url: String,
    /// When the pending request lapses, in milliseconds since the epoch.
    pub expires_at_ms: u64,
}

impl fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalRequest")
            .field("request_id", &self.request_id)
            .field("statement_b64url_len", &self.statement_b64url.len())
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// A refusal the vault gave as a result error, `{code, class}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultError {
    /// The contract's code: `not_found`, `limit_exceeded`, `secret_setup_incomplete`,
    /// `pending_limit`, `store_error`, `approval_invalid`, and the rest of §15's table.
    pub code: String,
    /// `permanent`, `transient` or `context_overflow`, when the code has a class.
    pub class: Option<String>,
}

impl VaultError {
    /// Whether a retry can change the answer.
    pub fn is_transient(&self) -> bool {
        matches!(self.class.as_deref(), Some("transient"))
    }
}

/// A refusal the vault gave as a transport error frame, `{code, message}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultTransportError {
    /// `invalid_params` and the other transport codes.
    pub code: String,
    /// The vault's fixed message, which never echoes the request.
    pub message: String,
}

/// The answer to one `secret.use` call.
#[derive(Debug)]
pub enum SecretUseAnswer {
    /// One of the three success shapes.
    Reply(SecretUseReply),
    /// A result error frame.
    Refused(VaultError),
    /// A transport error frame.
    Rejected(VaultTransportError),
}

/// The base64url engine the contract uses: strict and unpadded.
fn b64url() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Read `key_b64url`, which is strict unpadded base64url (nonzero trailing
/// bits included).
fn decode_key(text: &str) -> Result<Vec<u8>, ReplyError> {
    b64url()
        .decode(text)
        .map_err(|_| ReplyError::BadKeyEncoding)
}

/// The canonical 26-character Crockford ULID the vault's ids use: 26 base32
/// characters from the 32-symbol Crockford alphabet, uppercase, with no
/// padding. The first character is bounded to `0..=7` because the leading
/// 128-bit timestamp field may not overflow.
fn is_ulid(text: &str) -> bool {
    const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = text.as_bytes();
    bytes.len() == 26
        && matches!(bytes[0], b'0'..=b'7')
        && bytes.iter().all(|byte| CROCKFORD.contains(byte))
}

/// Read a reply value as one of the three documented forms.
///
/// Each form's key set is exactly what the contract shows. An unknown key, a
/// missing key, or a `kind` the form does not carry is a protocol error rather
/// than a guess, because a sender that guessed could substitute the wrong bytes
/// at the wrong place.
pub fn parse_reply(value: &Value) -> Result<SecretUseReply, ReplyError> {
    let object = value.as_object().ok_or_else(|| ReplyError::Malformed {
        detail: "the reply is not an object".into(),
    })?;
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| ReplyError::Malformed {
            detail: "the reply has no status".into(),
        })?;
    match status {
        "released" => parse_released(object),
        "approval_required" => {
            expect_keys(
                object,
                &["status", "request_id", "statement_b64url", "expires_at_ms"],
            )?;
            let request_id = object
                .get("request_id")
                .and_then(Value::as_str)
                .filter(|id| is_ulid(id))
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "request_id is not a ULID".into(),
                })?;
            let statement_b64url = object
                .get("statement_b64url")
                .and_then(Value::as_str)
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "statement_b64url is missing".into(),
                })?;
            // The statement is base64url, and decoding it here proves the
            // envelope bytes the phone will sign are intact.
            b64url()
                .decode(statement_b64url)
                .map_err(|_| ReplyError::Malformed {
                    detail: "statement_b64url is not strict unpadded base64url".into(),
                })?;
            let expires_at_ms = object
                .get("expires_at_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "expires_at_ms is not an unsigned integer".into(),
                })?;
            Ok(SecretUseReply::ApprovalRequired(ApprovalRequest {
                request_id: request_id.to_string(),
                statement_b64url: statement_b64url.to_string(),
                expires_at_ms,
            }))
        }
        other => Err(ReplyError::Malformed {
            detail: format!("unknown status {}", other.len()),
        }),
    }
}

/// Check that `object` carries exactly `keys`, so a contract change shows up as
/// a refusal instead of silently dropped or extra data.
fn expect_keys(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Result<(), ReplyError> {
    for key in keys {
        if !object.contains_key(*key) {
            return Err(ReplyError::Malformed {
                detail: "a documented reply key is missing".into(),
            });
        }
    }
    if object.len() != keys.len() {
        return Err(ReplyError::Malformed {
            detail: "the reply carries a key the contract does not show".into(),
        });
    }
    Ok(())
}

/// Distinguish the API-key form from the login form by their `kind`.
fn parse_released(object: &serde_json::Map<String, Value>) -> Result<SecretUseReply, ReplyError> {
    match object.get("kind").and_then(Value::as_str) {
        Some("agent_api_key") => {
            expect_keys(object, &["status", "kind", "key_b64url", "location"])?;
            let location = object
                .get("location")
                .and_then(Value::as_str)
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "location is missing".into(),
                })?;
            let key_b64url = object
                .get("key_b64url")
                .and_then(Value::as_str)
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "key_b64url is missing".into(),
                })?;
            Ok(SecretUseReply::ReleasedApiKey(ReleasedApiKey {
                location: location.to_string(),
                bytes: crate::secret::vault::Material::new(decode_key(key_b64url)?),
            }))
        }
        Some("site_login") => {
            let totp = object.get("totp").cloned();
            let keys: Vec<&str> = match &totp {
                Some(_) => vec!["status", "kind", "username", "password", "totp"],
                None => vec!["status", "kind", "username", "password"],
            };
            expect_keys(object, &keys)?;
            let username = object
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "username is missing".into(),
                })?;
            let password = object
                .get("password")
                .and_then(Value::as_str)
                .ok_or_else(|| ReplyError::Malformed {
                    detail: "password is missing".into(),
                })?;
            let totp = match totp {
                None | Some(Value::Null) => None,
                Some(Value::String(code)) => Some(code),
                Some(_) => {
                    return Err(ReplyError::Malformed {
                        detail: "totp is not a string".into(),
                    })
                }
            };
            Ok(SecretUseReply::ReleasedLogin(ReleasedLogin {
                username: username.to_string(),
                password: crate::secret::vault::Material::new(password.as_bytes().to_vec()),
                totp,
            }))
        }
        Some(other) => Err(ReplyError::Malformed {
            detail: format!("released reply kind {}", other.len()),
        }),
        None => Err(ReplyError::Malformed {
            detail: "a released reply carries no kind".into(),
        }),
    }
}

/// A result error frame `{"error":{"code":…,"class":…}}`, as §15's code table
/// spells it.
#[derive(Debug, Deserialize)]
struct ResultError {
    error: ResultErrorBody,
}

#[derive(Debug, Deserialize)]
struct ResultErrorBody {
    code: String,
    #[serde(default)]
    class: Option<String>,
}

/// A transport error frame `{"code":…,"message":…}` with no class (§1a).
#[derive(Debug, Deserialize)]
struct TransportError {
    code: String,
    message: String,
}

/// Read a whole `secret.use` reply body (`{"result": …}` or an error object).
///
/// This is the one place the wire shapes are interpreted, so the client and the
/// tests read replies by the same rules.
pub fn parse_answer(body: &Value) -> Result<SecretUseAnswer, ReplyError> {
    if let Some(result) = body.get("result") {
        if result.get("error").is_some() {
            let error: ResultError =
                serde_json::from_value(result.clone()).map_err(|_| ReplyError::Malformed {
                    detail: "the result error does not match the contract".into(),
                })?;
            return Ok(SecretUseAnswer::Refused(VaultError {
                code: error.error.code,
                class: error.error.class,
            }));
        }
        return parse_reply(result).map(SecretUseAnswer::Reply);
    }
    if let Ok(error) = serde_json::from_value::<TransportError>(body.clone()) {
        return Ok(SecretUseAnswer::Rejected(VaultTransportError {
            code: error.code,
            message: error.message,
        }));
    }
    Err(ReplyError::Malformed {
        detail: "the body is neither a result nor a transport error".into(),
    })
}
