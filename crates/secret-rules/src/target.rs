//! M1, M2 and M4: which request may carry a key, and at which path.
//!
//! Host matching is exact on the lowercased, IDNA-normalized ASCII host. Path
//! matching works on whole segments after removing dot segments and decoding
//! percent-encoded unreserved characters. Every escape this crate decodes is
//! also what the emitted path uses, so the path sent upstream is the normalized
//! path that matched, never the client's original bytes.

use crate::placeholder::hex_value;

/// Why a request does not match a rule's target (M1, M2, M4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRefusal {
    /// The scheme is not HTTPS.
    SchemeNotHttps,
    /// The host is not the declared host.
    HostMismatch,
    /// The port is not the declared port.
    PortMismatch,
    /// The path is not under the declared prefix.
    PathOutsidePrefix,
    /// A segment contains `%2F`, `%5C`, `\` or `;`.
    AmbiguousPathSeparator,
    /// The path has an empty segment (`//`).
    EmptySegment,
    /// The request target is not origin-form or a supported absolute-form.
    NotOriginForm,
    /// The request target cannot be normalized.
    MalformedPath,
    /// The request's authority differs from the connection's CONNECT authority
    /// (M4).
    AuthorityMismatch,
    /// An authority cannot be parsed.
    MalformedAuthority,
    /// The path carries a fragment, which no server receives.
    FragmentInTarget,
}

/// A normalized path: the value used for matching, and the bytes sent
/// upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedPath {
    /// Decoded path, used for matching.
    pub value: String,
    /// Percent-encoded RFC 3986 wire form, which is what goes upstream.
    pub wire: Vec<u8>,
}

/// A rule's target.
#[derive(Debug, Clone, Copy)]
pub struct RuleTarget<'a> {
    pub host: &'a str,
    pub port: u16,
    pub path_prefix: &'a str,
}

/// One request's destination.
#[derive(Debug, Clone)]
pub struct RequestTarget<'a> {
    /// The host the request is addressed to (the CONNECT authority's host, or
    /// the fetch URL's).
    pub host: String,
    pub port: u16,
    pub scheme: &'a str,
    /// The request target as received.
    pub target: &'a str,
}

impl<'a> RequestTarget<'a> {
    /// Match this request against `rule` (M1, M2).
    pub fn matches(&self, rule: &RuleTarget<'_>) -> Result<NormalizedPath, TargetRefusal> {
        if !self.scheme.eq_ignore_ascii_case("https") {
            return Err(TargetRefusal::SchemeNotHttps);
        }
        if canonical_host(&self.host)? != canonical_host(rule.host)? {
            return Err(TargetRefusal::HostMismatch);
        }
        if self.port != rule.port {
            return Err(TargetRefusal::PortMismatch);
        }
        let path = normalize_request_target(self.target)?;
        let prefix = normalize_path_prefix(rule.path_prefix)?;
        if !path_covers(&path.value, &prefix) {
            return Err(TargetRefusal::PathOutsidePrefix);
        }
        Ok(path)
    }
}

/// Normalize a host for comparison: validate, IDNA-map to ASCII, lowercase.
pub fn canonical_host(host: &str) -> Result<String, TargetRefusal> {
    if host.is_empty() {
        return Err(TargetRefusal::MalformedAuthority);
    }
    // A bracketed IPv6 literal is already ASCII and must not be IDNA-mapped.
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        if inner.is_empty()
            || !inner
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || ch == ':' || ch == '.')
        {
            return Err(TargetRefusal::MalformedAuthority);
        }
        return Ok(format!("[{}]", inner.to_ascii_lowercase()));
    }
    // A bare IPv6 literal is not a domain name; only the bracketed form is.
    if host.contains(':') {
        return Err(TargetRefusal::MalformedAuthority);
    }
    // A trailing dot is a DNS spelling AFT does not model; a host that carries
    // one is not the same authority as one that does not.
    if host.ends_with('.') {
        return Err(TargetRefusal::MalformedAuthority);
    }
    let (domain, trailing_dot) = (host, false);
    let ascii = idna::domain_to_ascii(domain).map_err(|_| TargetRefusal::MalformedAuthority)?;
    if ascii.is_empty() {
        return Err(TargetRefusal::MalformedAuthority);
    }
    let mut out = ascii.to_ascii_lowercase();
    if trailing_dot {
        out.push('.');
    }
    Ok(out)
}

/// Normalize a request target into its path and query wire form.
///
/// An absolute-form target (`https://host/path`) is accepted only when its
/// scheme is HTTPS; its authority is compared separately by
/// [`authority_matches_connect`].
///
/// A `#` anywhere in the target refuses it, in the query as much as in the
/// path: a server never receives a fragment, so a target that carries one is
/// not the target that would be forwarded.
pub fn normalize_request_target(target: &str) -> Result<NormalizedPath, TargetRefusal> {
    if target.contains('#') {
        return Err(TargetRefusal::FragmentInTarget);
    }
    let origin_form = match strip_https(target) {
        Some(rest) => {
            let path_start = rest.find(['/', '?']).unwrap_or(rest.len());
            let tail = &rest[path_start..];
            if tail.is_empty() {
                "/".to_string()
            } else if tail.starts_with('?') {
                format!("/{tail}")
            } else {
                tail.to_string()
            }
        }
        None => {
            if !target.starts_with('/') {
                return Err(TargetRefusal::NotOriginForm);
            }
            target.to_string()
        }
    };
    let (path_part, query_part) = match origin_form.find('?') {
        Some(at) => (&origin_form[..at], Some(&origin_form[at..])),
        None => (origin_form.as_str(), None),
    };
    let path = canonicalize_path(path_part)?;
    let mut wire = path.wire.clone();
    if let Some(query) = query_part {
        wire.extend_from_slice(query.as_bytes());
    }
    Ok(NormalizedPath {
        value: path.value,
        wire,
    })
}

fn strip_https(value: &str) -> Option<&str> {
    value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("HTTPS://"))
}

/// A decoded path and its wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPath {
    pub value: String,
    pub wire: Vec<u8>,
}

/// Split a path into decoded segments after removing dot segments.
///
/// The wire form percent-encodes every byte outside the RFC 3986 unreserved
/// set, so the emitted bytes are exactly the value that matched.
pub fn canonicalize_path(path: &str) -> Result<CanonicalPath, TargetRefusal> {
    if !path.starts_with('/') {
        return Err(TargetRefusal::NotOriginForm);
    }
    // A fragment is not part of the path a server receives; treating `#` as a
    // path byte would match one location and forward another.
    if path.contains('#') {
        return Err(TargetRefusal::FragmentInTarget);
    }
    let bytes = path.as_bytes();
    if bytes.len() == 1 {
        return Ok(CanonicalPath {
            value: "/".to_string(),
            wire: b"/".to_vec(),
        });
    }
    let mut segments: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'/' => segments.push(std::mem::take(&mut current)),
            b'\\' | b';' => return Err(TargetRefusal::AmbiguousPathSeparator),
            b'%' => {
                let (decoded, len) = decode_escape(bytes, index + 1)?;
                if decoded == b'/' || decoded == b'\\' || decoded == b';' || decoded == b'%' {
                    // Separators and nested escapes would let a second decoding
                    // layer read a different path than the one approved here.
                    return Err(TargetRefusal::AmbiguousPathSeparator);
                }
                // A dot written as an escape (`%2E`) is a dot segment once
                // decoded. It refuses for the same reason `..` does: AFT and
                // the server would each normalise a different spelling, and a
                // server that decodes twice would resolve the segment away.
                if decoded == b'.' {
                    return Err(TargetRefusal::AmbiguousPathSeparator);
                }
                current.push(decoded);
                index += len;
                continue;
            }
            byte => current.push(byte),
        }
        index += 1;
    }
    segments.push(current);

    // RFC 3986 section 5.2.4: remove dot segments. A segment holding an empty
    // segment elsewhere in the path refuses the request instead of guessing
    // which one the operator meant.
    let mut output: Vec<Vec<u8>> = Vec::new();
    let last_index = segments.len() - 1;
    for (position, segment) in segments.iter().enumerate() {
        let is_last = position == last_index;
        match segment.as_slice() {
            b"." => {
                if is_last {
                    output.push(Vec::new());
                }
            }
            b".." => {
                output.pop();
                if is_last {
                    output.push(Vec::new());
                }
            }
            _ => {
                if segment.is_empty() {
                    // A repeated separator, or a segment that only becomes
                    // empty after dot removal, would let a path mean the same
                    // thing in two spellings.
                    return Err(TargetRefusal::EmptySegment);
                }
                output.push(segment.clone());
            }
        }
    }

    // An absolute path starts with its separator; every later segment is
    // separated by one too. The empty segment left by a trailing dot segment is
    // the path's own end rather than another segment, so it is dropped here and
    // re-appended as the trailing separator.
    while output.last().is_some_and(|segment| segment.is_empty()) {
        output.pop();
    }
    let mut value = String::from("/");
    let mut wire = vec![b'/'];
    for (position, segment) in output.iter().enumerate() {
        if position > 0 {
            value.push('/');
            wire.push(b'/');
        }
        value.push_str(std::str::from_utf8(segment).map_err(|_| TargetRefusal::MalformedPath)?);
        for byte in segment {
            if crate::material::is_unreserved(*byte) {
                wire.push(*byte);
            } else {
                wire.push(b'%');
                wire.extend_from_slice(hex_pair(*byte).as_bytes());
            }
        }
    }
    Ok(CanonicalPath { value, wire })
}

/// Normalize a declared path prefix.
///
/// The prefix must already be in its normalized spelling, which is then
/// returned unchanged; `/` is the root. A prefix that would broaden to a
/// different location (`//`, `/.`, `/v1/..`, a trailing separator, or a prefix
/// that is not absolute) refuses instead of silently matching `/`.
pub fn normalize_path_prefix(prefix: &str) -> Result<String, TargetRefusal> {
    if !prefix.starts_with('/') {
        return Err(TargetRefusal::MalformedPath);
    }
    // A prefix is compared as written. Dot segments, empty segments and a
    // trailing separator all refuse: each of them is a second spelling for a
    // location, and AFT must not guess which one the operator meant.
    let path = canonicalize_path(prefix)?;
    if path.value != prefix {
        return Err(TargetRefusal::MalformedPath);
    }
    Ok(path.value)
}

/// Uppercase hex of one byte, without the format machinery's locale and
/// width rules.
pub(crate) fn hex_pair(byte: u8) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let pair = [DIGITS[(byte >> 4) as usize], DIGITS[(byte & 0x0f) as usize]];
    String::from_utf8_lossy(&pair).into_owned()
}

/// Decode one `%XX` escape whose two hex digits start at `start`.
fn decode_escape(bytes: &[u8], start: usize) -> Result<(u8, usize), TargetRefusal> {
    let high = bytes.get(start).copied().and_then(hex_value);
    let low = bytes.get(start + 1).copied().and_then(hex_value);
    match (high, low) {
        (Some(high), Some(low)) => Ok(((high << 4) | low, 3)),
        _ => Err(TargetRefusal::MalformedPath),
    }
}

/// True when `path` is `prefix` or lies under it on whole segments.
///
/// `prefix` must be a declared prefix in its normalized spelling (see
/// [`normalize_path_prefix`]); `/` covers every absolute path. Any other
/// spelling (`//`, `/v1/`, `/v1/..`, an empty or relative prefix) covers
/// nothing, so a caller that skips validation cannot widen a prefix to the
/// root by accident.
pub fn path_covers(path: &str, prefix: &str) -> bool {
    let Ok(prefix) = normalize_path_prefix(prefix) else {
        return false;
    };
    if prefix == "/" {
        return path.starts_with('/');
    }
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// The CONNECT authority of a tunnelled connection (M4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectAuthority {
    pub host: String,
    pub port: u16,
}

impl ConnectAuthority {
    /// Parse `host:port`, with bracketed IPv6 supported.
    ///
    /// Userinfo and any text after the host that is not a port refuse the
    /// authority: the value AFT authorizes must be the value a client would
    /// connect to.
    pub fn parse(authority: &str) -> Result<Self, TargetRefusal> {
        if authority.is_empty() || authority.contains('@') {
            return Err(TargetRefusal::MalformedAuthority);
        }
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let Some((inner, tail)) = rest.split_once(']') else {
                return Err(TargetRefusal::MalformedAuthority);
            };
            let port = match tail {
                "" => return Err(TargetRefusal::MalformedAuthority),
                tail => match tail.strip_prefix(':') {
                    Some(port) => port,
                    None => return Err(TargetRefusal::MalformedAuthority),
                },
            };
            (format!("[{inner}]"), port)
        } else {
            let Some((host, port)) = authority.rsplit_once(':') else {
                return Err(TargetRefusal::MalformedAuthority);
            };
            (host.to_string(), port)
        };
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(TargetRefusal::MalformedAuthority);
        }
        let port: u16 = port
            .parse()
            .map_err(|_| TargetRefusal::MalformedAuthority)?;
        Ok(Self {
            host: canonical_host(&host)?,
            port,
        })
    }

    /// Value written into the outgoing `Host` header and `:authority`.
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// M4: a request's own authority must equal the connection's CONNECT
/// authority.
///
/// The check runs on every request of a connection, so a keep-alive request or
/// a second HTTP/2 stream cannot address another host through an approved
/// tunnel. An absent header is fine; a present one that disagrees refuses.
pub fn authority_matches_connect(
    connect: &ConnectAuthority,
    host_header: Option<&str>,
    authority_header: Option<&str>,
    target: &str,
) -> Result<(), TargetRefusal> {
    for value in [host_header, authority_header].into_iter().flatten() {
        let parsed = parse_authority_with_default(value, connect.port)?;
        if parsed != (connect.host.clone(), connect.port) {
            return Err(TargetRefusal::AuthorityMismatch);
        }
    }
    if strip_https(target).is_some() {
        let rest = strip_https(target).unwrap_or_default();
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let parsed = parse_authority_with_default(&rest[..authority_end], 443)?;
        if parsed != (connect.host.clone(), connect.port) {
            return Err(TargetRefusal::AuthorityMismatch);
        }
    }
    Ok(())
}

fn parse_authority_with_default(
    value: &str,
    default_port: u16,
) -> Result<(String, u16), TargetRefusal> {
    if value.is_empty() || value.contains('@') {
        return Err(TargetRefusal::MalformedAuthority);
    }
    match value.rsplit_once(':') {
        // No port: the default applies.
        None => Ok((canonical_host(value)?, default_port)),
        Some(_) => {
            let parsed = ConnectAuthority::parse(value)?;
            Ok((parsed.host, parsed.port))
        }
    }
}
