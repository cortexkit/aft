//! M3: every redirect is a new request, evaluated from scratch.
//!
//! A redirect is not a continuation of the request that caused it. The new
//! location gets its own scheme, host, port and path check, so a hop to
//! another host, another scheme or another port never receives the key.

use crate::location::RequestRefusal;
use crate::placeholder::mentions_label_loose;
use crate::target::{
    canonical_host, normalize_request_target, path_covers, RequestTarget, RuleTarget, TargetRefusal,
};
use crate::LabelRule;

/// How a redirect hop was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOutcome {
    /// At least one rule covers the hop; substitute as for a fresh request.
    Authorized,
    /// No rule covers the hop; send the placeholder literally.
    NotAuthorized,
}

/// Resolve a `Location` header value against `current`, then parse it.
///
/// A relative reference inherits the current request's scheme, host and port,
/// but is still a new request under M3.
pub fn resolve_location(
    location: &str,
    current: &RequestTarget<'_>,
) -> Result<String, TargetRefusal> {
    let location = location.trim();
    if location.is_empty() {
        return Err(TargetRefusal::MalformedAuthority);
    }
    if location.starts_with("https://") || location.starts_with("HTTPS://") {
        return Ok(location.to_string());
    }
    if location.contains("://") {
        // A non-HTTPS scheme resolves as-is; evaluation refuses it later.
        return Ok(location.to_string());
    }
    if let Some(authority_and_path) = location.strip_prefix("//") {
        return Ok(format!("https://{authority_and_path}"));
    }
    let port = if current.port == 443 {
        String::new()
    } else {
        format!(":{}", current.port)
    };
    let path = if location.starts_with('/') {
        location.to_string()
    } else {
        let base = current.target.split('?').next().unwrap_or("/");
        let directory = match base.rfind('/') {
            Some(at) => &base[..=at],
            None => "/",
        };
        format!("{directory}{location}")
    };
    Ok(format!("https://{}{port}{path}", current.host))
}

/// Split an absolute URL into the request target it implies.
///
/// Returns `(scheme, host, port, origin_form_target)`.
pub fn split_absolute(url: &str) -> Result<(String, String, u16, String), TargetRefusal> {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => return Err(TargetRefusal::NotOriginForm),
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let (host, port) = split_host_port(authority)?;
    let target = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Ok((scheme, host, port, target))
}

/// Split `host[:port]` from an authority.
///
/// The parse is strict on purpose: userinfo (`user@host`) and any text that is
/// neither the host nor a decimal port refuse the URL. A lenient parse here
/// would let the authority AFT authorizes differ from the one a browser-style
/// URL consumer connects to.
fn split_host_port(authority: &str) -> Result<(String, u16), TargetRefusal> {
    if authority.is_empty() || authority.contains('@') {
        return Err(TargetRefusal::MalformedAuthority);
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((inner, tail)) = rest.split_once(']') else {
            return Err(TargetRefusal::MalformedAuthority);
        };
        let port = match tail {
            "" => 443,
            tail => match tail.strip_prefix(':') {
                Some(port) => parse_port(port)?,
                // Anything else after the bracket is not part of the authority.
                None => return Err(TargetRefusal::MalformedAuthority),
            },
        };
        return Ok((canonical_host(&format!("[{inner}]"))?, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((canonical_host(host)?, parse_port(port)?)),
        None => Ok((canonical_host(authority)?, 443)),
    }
}

fn parse_port(port: &str) -> Result<u16, TargetRefusal> {
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TargetRefusal::MalformedAuthority);
    }
    port.parse().map_err(|_| TargetRefusal::MalformedAuthority)
}

/// How one label's key behaves across a redirect hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectDecision {
    /// The label whose rule was evaluated.
    pub label: String,
    /// Whether this label's key may be substituted at the hop.
    pub outcome: RedirectOutcome,
    /// Why this label was not authorized, when it was not.
    pub refusal: Option<RequestRefusal>,
}

/// Decide, **per label**, whether a redirect hop may receive that label's key.
///
/// A hop is authorized for one label only when that label's own rule covers it.
/// There is deliberately no aggregate verdict: a hop that matches label A says
/// nothing about whether label B's key may be sent there.
pub fn evaluate_redirect(rules: &[LabelRule], hop: &RequestTarget<'_>) -> Vec<RedirectDecision> {
    rules
        .iter()
        .map(|rule| {
            // A rule that is not valid authorizes nothing: the redirect path
            // gets the same check as the request path.
            if let Some(err) = crate::location::validate_rule(rule) {
                return RedirectDecision {
                    label: rule.label.clone(),
                    outcome: RedirectOutcome::NotAuthorized,
                    refusal: Some(err),
                };
            }
            let target = RuleTarget {
                host: &rule.host,
                port: rule.port,
                path_prefix: &rule.path_prefix,
            };
            match hop.matches(&target) {
                Ok(_) => RedirectDecision {
                    label: rule.label.clone(),
                    outcome: RedirectOutcome::Authorized,
                    refusal: None,
                },
                Err(err) => RedirectDecision {
                    label: rule.label.clone(),
                    outcome: RedirectOutcome::NotAuthorized,
                    refusal: Some(RequestRefusal::TargetMismatch(err)),
                },
            }
        })
        .collect()
}

/// True when a `Location` value still mentions the label's placeholder, so a
/// redirect to an authorized path is substituted exactly like a client
/// request.
pub fn location_mentions(location: &str, label: &str) -> bool {
    mentions_label_loose(location, label)
}

/// True when `path` stays under `prefix` after normalization.
///
/// `prefix` is used only in its normalized spelling; any other spelling
/// (including `//`) covers nothing.
pub fn path_is_covered(path: &str, prefix: &str) -> bool {
    normalize_request_target(path)
        .map(|normalized| path_covers(&normalized.value, prefix))
        .unwrap_or(false)
}
