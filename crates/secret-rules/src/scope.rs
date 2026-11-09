//! Vault scope grammar and matching. Unlike proxy wire-path normalization in
//! `target`, scope matching compares literal paths without percent-decoding.
//! Input creation checks the pinned PSL; stored scopes never consult it.

use serde_json::{json, Map, Value};

mod psl;
pub use psl::{PslSnapshot, PSL_SNAPSHOT};

pub const MAX_RULES: usize = 8;
pub const MAX_ORIGINS: usize = 4;
pub const MAX_HOST_BYTES: usize = 253;
pub const MAX_PREFIX_BYTES: usize = 1024;

/// Refusals map directly to the vault's two scope error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRefusal {
    InvalidParams,
    LimitExceeded,
}

impl ScopeRefusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidParams => "invalid_params",
            Self::LimitExceeded => "limit_exceeded",
        }
    }
}

use ScopeRefusal::{InvalidParams, LimitExceeded};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Rule {
    host: String,
    prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    ApiKey {
        rules: Vec<Rule>,
        extended: bool,
    },
    SiteLogin {
        origins: Vec<String>,
        extended: bool,
    },
}

/// A validated, canonically ordered scope, without location or secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretScope(Scope);

/// One concrete `secret.use` target. A target cannot change the scope's kind.
#[derive(Debug, Clone, Copy)]
pub enum UseTarget<'a> {
    ApiKey {
        scheme: &'a str,
        host: &'a str,
        port: u16,
        path: &'a str,
    },
    SiteLogin {
        origin: &'a str,
    },
}

impl SecretScope {
    /// Parse scope fields only (not location or transport metadata). Validation
    /// runs in sent order before canonical sorting and the creation-only PSL check.
    pub fn parse_input(kind: &str, fields: &Value) -> Result<Self, ScopeRefusal> {
        parse(kind, fields, false)
    }

    /// Decode emitted scope fields, requiring canonical order and explicit rule
    /// prefixes. An empty emitted prefix means absent. The PSL is not consulted.
    pub fn decode_stored(kind: &str, fields: &Value) -> Result<Self, ScopeRefusal> {
        parse(kind, fields, true)
    }

    pub fn is_extended(&self) -> bool {
        match &self.0 {
            Scope::ApiKey { extended, .. } | Scope::SiteLogin { extended, .. } => *extended,
        }
    }

    /// Canonical emitted scope fields. The caller owns location and encodings.
    pub fn to_value(&self) -> Value {
        match &self.0 {
            Scope::ApiKey {
                rules,
                extended: true,
            } => json!({"rules": rules.iter()
                .map(|r| json!({"host": r.host, "path_prefix": r.prefix})).collect::<Vec<_>>() }),
            Scope::ApiKey {
                rules,
                extended: false,
            } => {
                json!({"host": rules[0].host, "path_prefix": rules[0].prefix})
            }
            Scope::SiteLogin {
                origins,
                extended: true,
            } => json!({"origins": origins}),
            Scope::SiteLogin {
                origins,
                extended: false,
            } => json!({"origin": origins[0]}),
        }
    }

    pub fn matches(&self, target: UseTarget<'_>) -> bool {
        match (&self.0, target) {
            (
                Scope::ApiKey { rules, .. },
                UseTarget::ApiKey {
                    scheme,
                    host,
                    port,
                    path,
                },
            ) => {
                scheme == "https"
                    && port == 443
                    && valid_host(host)
                    && rules.iter().any(|rule| {
                        let host_matches = if let Some(base) = rule.host.strip_prefix("*.") {
                            host.strip_suffix(base)
                                .and_then(|s| s.strip_suffix('.'))
                                .is_some_and(|label| !label.is_empty() && !label.contains('.'))
                        } else {
                            rule.host == host
                        };
                        host_matches
                            && (rule.prefix.is_empty()
                                || path == rule.prefix
                                || path
                                    .strip_prefix(&rule.prefix)
                                    .is_some_and(|tail| tail.starts_with('/')))
                    })
            }
            (Scope::SiteLogin { origins, .. }, UseTarget::SiteLogin { origin }) => {
                canonical_origin(origin).is_ok_and(|target| {
                    origins
                        .iter()
                        .any(|stored| canonical_origin(stored) == Ok(target))
                })
            }
            _ => false,
        }
    }
}

fn parse(kind: &str, fields: &Value, stored: bool) -> Result<SecretScope, ScopeRefusal> {
    let fields = fields.as_object().ok_or(InvalidParams)?;
    match kind {
        "agent_api_key" => {
            let extended = fields.contains_key("rules");
            let entries: Vec<&Map<String, Value>> = if extended {
                keys(fields, &["rules"])?;
                let entries = fields["rules"].as_array().ok_or(InvalidParams)?;
                // Shape precedes count, including malformed entries after the cap.
                entries
                    .iter()
                    .map(|v| {
                        let entry = v.as_object().ok_or(InvalidParams)?;
                        rule_shape(entry, stored)?;
                        Ok(entry)
                    })
                    .collect::<Result<_, ScopeRefusal>>()?
            } else {
                rule_shape(fields, false)?;
                vec![fields]
            };
            if entries.is_empty() {
                return Err(InvalidParams);
            }
            if entries.len() > MAX_RULES {
                return Err(LimitExceeded);
            }
            let mut rules = Vec::with_capacity(entries.len());
            for entry in entries {
                let host = entry["host"].as_str().ok_or(InvalidParams)?;
                let base = if extended {
                    host.strip_prefix("*.").unwrap_or(host)
                } else {
                    host
                };
                if !valid_host(base) {
                    return Err(InvalidParams);
                }
                if host.len() > MAX_HOST_BYTES {
                    return Err(LimitExceeded);
                }
                let prefix = entry.get("path_prefix").and_then(Value::as_str);
                let prefix = prefix.filter(|p| !(stored && p.is_empty()));
                if let Some(prefix) = prefix {
                    validate_prefix(prefix)?;
                }
                rules.push(Rule {
                    host: host.into(),
                    prefix: prefix.unwrap_or("").into(),
                });
            }
            if extended && rules.len() == 1 && !rules[0].host.starts_with("*.") {
                return Err(InvalidParams);
            }
            let sent = rules.clone();
            rules.sort();
            if rules.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(InvalidParams);
            }
            if stored && rules != sent {
                return Err(InvalidParams);
            }
            if !stored
                && rules
                    .iter()
                    .any(|r| r.host.strip_prefix("*.").is_some_and(psl::is_public_suffix))
            {
                return Err(InvalidParams);
            }
            Ok(SecretScope(Scope::ApiKey { rules, extended }))
        }
        "site_login" => {
            let extended = fields.contains_key("origins");
            let entries = if extended {
                keys(fields, &["origins"])?;
                fields["origins"]
                    .as_array()
                    .ok_or(InvalidParams)?
                    .iter()
                    .map(|v| v.as_str().ok_or(InvalidParams))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                keys(fields, &["origin"])?;
                vec![fields
                    .get("origin")
                    .and_then(Value::as_str)
                    .ok_or(InvalidParams)?]
            };
            if entries.is_empty() || (extended && entries.len() == 1) {
                return Err(InvalidParams);
            }
            if entries.len() > MAX_ORIGINS {
                return Err(LimitExceeded);
            }
            let mut canonical = Vec::with_capacity(entries.len());
            for entry in &entries {
                canonical.push(canonical_origin(entry)?);
            }
            canonical.sort();
            if canonical.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(InvalidParams);
            }
            let mut origins: Vec<String> = entries.iter().map(|s| (*s).into()).collect();
            origins.sort();
            if stored && !origins.iter().map(String::as_str).eq(entries) {
                return Err(InvalidParams);
            }
            Ok(SecretScope(Scope::SiteLogin { origins, extended }))
        }
        _ => Err(InvalidParams),
    }
}

fn keys(fields: &Map<String, Value>, allowed: &[&str]) -> Result<(), ScopeRefusal> {
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        Err(InvalidParams)
    } else {
        Ok(())
    }
}

fn rule_shape(fields: &Map<String, Value>, emitted: bool) -> Result<(), ScopeRefusal> {
    keys(fields, &["host", "path_prefix"])?;
    if !fields.get("host").is_some_and(Value::is_string)
        || fields.get("path_prefix").is_some_and(|v| !v.is_string())
        || (emitted && !fields.contains_key("path_prefix"))
    {
        return Err(InvalidParams);
    }
    Ok(())
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.parse::<std::net::IpAddr>().is_err()
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Validate a login origin and remove only an explicit default port. No URL
/// normalization is performed, and the original spelling can still be stored.
pub fn canonical_origin(origin: &str) -> Result<&str, ScopeRefusal> {
    let authority = origin.strip_prefix("https://").ok_or(InvalidParams)?;
    let host = authority.strip_suffix(":443").unwrap_or(authority);
    if !valid_host(host) {
        return Err(InvalidParams);
    }
    if host.len() > MAX_HOST_BYTES {
        return Err(LimitExceeded);
    }
    Ok(origin.strip_suffix(":443").unwrap_or(origin))
}

fn validate_prefix(prefix: &str) -> Result<(), ScopeRefusal> {
    if !prefix.starts_with('/') || prefix.ends_with('/') || prefix.contains("//") {
        return Err(InvalidParams);
    }
    let bytes = prefix.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let b = bytes[at];
        if b == b'%' {
            let hi = bytes
                .get(at + 1)
                .and_then(|b| crate::placeholder::hex_value(*b));
            let lo = bytes
                .get(at + 2)
                .and_then(|b| crate::placeholder::hex_value(*b));
            let decoded = hi.zip(lo).map(|(h, l)| h * 16 + l).ok_or(InvalidParams)?;
            if !(0x21..=0x7e).contains(&decoded) || b"/.%\\;".contains(&decoded) {
                return Err(InvalidParams);
            }
            at += 3;
        } else if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b) {
            at += 1;
        } else {
            return Err(InvalidParams);
        }
    }
    if prefix
        .split('/')
        .skip(1)
        .any(|s| matches!(s.split(';').next(), Some("." | "..")))
    {
        return Err(InvalidParams);
    }
    if prefix.len() > MAX_PREFIX_BYTES {
        return Err(LimitExceeded);
    }
    Ok(())
}
